# Gladia — STT (live / real-time, REST init + WebSocket)

Trait: `brisvox_providers::stt::SttProvider` (`SttInput::{Audio, Finalize}`,
`SttEvent::{Transcript{text,is_final,speech_final,from_finalize}, Error}`).

- Docs (all fetched 2026-10-08; everything below comes from these pages unless marked):
  - Init a live session (REST): https://docs.gladia.io/api-reference/v2/live/init.md
  - Live WebSocket (AsyncAPI-rendered reference): https://docs.gladia.io/api-reference/v2/live/websocket.md
    (the fetch tool read only the first 100000 of 120611 characters on the first call; the tail with
    `audio_chunk_ack` .. `end_session` was read with `offset=100000`)
  - AsyncAPI spec: https://docs.gladia.io/asyncapi.yaml
  - Stop recording payload: https://docs.gladia.io/api-reference/v2/live/action/stop-recording.md
  - Quickstart / protocol: https://docs.gladia.io/chapters/live-stt/quickstart.md and
    https://docs.gladia.io/chapters/live-stt/getting-started
  - Endpointing: https://docs.gladia.io/chapters/live-stt/features/endpointing.md
  - Partial transcripts: https://docs.gladia.io/chapters/live-stt/features/partial-transcripts.md
  - Recommended parameters by use case: https://docs.gladia.io/chapters/live-stt/recommended-parameters.md
  - Deepgram -> Gladia migration: https://docs.gladia.io/chapters/migrations/from-deepgram.md
  - Get result: https://docs.gladia.io/api-reference/v2/live/get.md
  - Delete live job: https://docs.gladia.io/api-reference/v2/live/delete.md
  - Concurrency / limits: https://docs.gladia.io/chapters/limits-and-specifications/concurrency.md
  - Supported languages: https://docs.gladia.io/chapters/language/supported-languages.md
  - Code switching: https://docs.gladia.io/chapters/language/code-switching.md
  - Custom vocabulary: https://docs.gladia.io/chapters/audio-intelligence/custom-vocabulary.md
  - Data retention: https://docs.gladia.io/chapters/limits-and-specifications/data-retention.md
  - WebSocket handling (support article, possibly dated): https://support.gladia.io/article/how-to-handle-the-websocket-connection
  - Doc index used to find pages: https://docs.gladia.io/llms.txt
  - Pricing: https://www.gladia.io/pricing

## Auth
- Init call (`POST /v2/live`): header `x-gladia-key: <GLADIA_API_KEY>` and `Content-Type: application/json`
  (the init page shows `x-gladia-key` as the auth header; that `Content-Type` is marked required: UNVERIFIED, send
  it anyway). The docs recommend calling init from a backend so the key stays private.
- WebSocket: **no API key**. The init response `url` carries a temporary session token:
  `wss://api.gladia.io/v2/live?token=<uuid>` (init reference: "The url will contain the temporary token to
  authenticate the session."; the quickstart example shows the token equal to the session `id`; AsyncAPI: "session
  token passed as the `token` query parameter"; no `securitySchemes` defined). The URL is therefore a secret: never
  log it (brisvox: redact the query).
- Token lifetime / how long after init the socket must be opened: UNVERIFIED (not stated on any fetched page).
- Failed init: JSON error body with `timestamp`, `path`, `request_id`, `statusCode`, `message`; 400 adds
  `validation_errors` (array of strings; 400 example message `"Content-Type is missing Multipart Boundary."`);
  401 = "Missing or insufficient permissions" (example message `"gladia key not found"`);
  422 = invalid parameters (example message `"Invalid parameter"`). 429 = concurrency/rate limit (see Limits).

## Endpoint(s) / regions
- Init: `POST https://api.gladia.io/v2/live` (optional query `region`, values `us-west` | `eu-west`; "The region used
  to process the audio"). Example in the docs: `POST https://api.gladia.io/v2/live?region=us-west`.
- The returned `url` is `wss://api.gladia.io/v2/live?token=...` in all doc examples. Whether `region` changes the
  hostname of the returned `url`: UNVERIFIED (docs only show the global host). Default region when `region` is
  omitted: UNVERIFIED.
- Result retrieval after the session: `GET https://api.gladia.io/v2/live/{id}` (header `x-gladia-key`; response has
  `status` queued/processing/done/error, `request_params`, `result`, `file`, `created_at`, `completed_at`,
  `custom_metadata`; 200/401/404). Not needed for the brisvox adapter (live transcripts arrive over the socket).
- Flow: init (REST) -> open WebSocket -> stream audio -> handle messages -> `stop_recording` or close 1000 ->
  optional GET. Brisvox consequence: `connect()` costs one HTTPS round trip before the WebSocket handshake.
  `warm()` can resolve DNS / build the TLS config; pre-creating sessions is unsafe until token lifetime is known
  (UNVERIFIED).

## Init request (`POST /v2/live`) — body fields
All optional per the reference. Defaults as stated there.
| Field | Type / default / values |
|---|---|
| `encoding` | string, default `wav/pcm`; enum `wav/pcm`, `wav/alaw`, `wav/ulaw`. PCM supports 8/16/24/32 bit, A-law and mu-law are 8 bit. "WAV headers aren't needed for raw audio" |
| `bit_depth` | number, default `16`; `8`, `16`, `24`, `32` |
| `sample_rate` | number, default `16000`; `8000`, `16000`, `32000`, `44100`, `48000` |
| `channels` | integer, default `1`, min 1, max 8 (multi-channel billed as duration x channels) |
| `model` | string, default and only value `solaria-1` ("live transcription supports `solaria-1` only") |
| `endpointing` | number seconds, default `0.05`, min `0.01`, max `10` |
| `maximum_duration_without_endpointing` | number seconds, default `5`, min `5`, max `60` |
| `language_config` | `{languages: [codes], code_switching: bool}`; `languages` default `[]` ("If one language is set, it will be used for the transcription. Otherwise, language will be auto-detected by the model": one code = that language; none or several = auto-detect); `code_switching` default `false` ("If true, language will be auto-detected on each utterance. Otherwise, language will be auto-detected on first utterance"); that it is ignored when exactly one language is set: UNVERIFIED |
| `pre_processing` | `{audio_enhancer: bool (default false), speech_threshold: number 0..1 (default 0.6; "A value close to 1 will apply stricter thresholds")}` |
| `realtime_processing` | `custom_vocabulary` (+`custom_vocabulary_config`), `custom_spelling` (+`custom_spelling_config`), `translation` (+`translation_config`), `named_entity_recognition`, `sentiment_analysis` (all default false) |
| `post_processing` | `summarization` (+`summarization_config.type`: `general` default / `bullet_points` / `concise`); `chapterization` deprecated and ignored |
| `messages_config` | which WebSocket messages to receive (below) |
| `callback`, `callback_config` | `callback` default false; `callback_config` has `url` and the same receive flags with different defaults (`receive_partial_transcripts` false, `receive_final_transcripts` true, `receive_speech_events` false, `receive_acknowledgments` false, `receive_errors` false, `receive_lifecycle_events` true, others true) — unused by brisvox |
| `custom_metadata` | free-form object attached to the session |

`messages_config` flags (all booleans) and defaults: `receive_partial_transcripts` **false**,
`receive_final_transcripts` true, `receive_speech_events` true, `receive_pre_processing_events` true,
`receive_realtime_processing_events` true, `receive_post_processing_events` true, `receive_acknowledgments` true,
`receive_errors` true, `receive_lifecycle_events` **false**.
=> brisvox must set `receive_partial_transcripts: true` to get interims. Whether switching off acknowledgments /
post-processing events is safe and reduces traffic: the flags exist; effect on latency is UNVERIFIED.

`encoding`, `sample_rate`, `bit_depth`, `channels` "describe the source audio and each configured value must match
the audio being sent" (getting-started; the quickstart words it "must match the audio you send"); the server uses
them to parse chunks. 16000 is "one supported configuration, not the only supported value".

Request example from the init reference (curl, trimmed to the relevant fields):
```bash
curl -X POST "https://api.gladia.io/v2/live?region=us-west" \
  -H "x-gladia-key: YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "encoding": "wav/pcm",
    "bit_depth": 16,
    "sample_rate": 16000,
    "channels": 1,
    "model": "solaria-1",
    "endpointing": 0.05,
    "maximum_duration_without_endpointing": 5,
    "language_config": { "languages": ["en"], "code_switching": false },
    "pre_processing": { "audio_enhancer": true, "speech_threshold": 0.6 },
    "messages_config": {
      "receive_partial_transcripts": true,
      "receive_final_transcripts": true,
      "receive_lifecycle_events": true
    },
    "callback": false
  }'
```
Recommended brisvox body (derived from the docs, not copied; Turkish, PCM16 mono):
```json
{"encoding":"wav/pcm","bit_depth":16,"sample_rate":16000,"channels":1,"model":"solaria-1",
 "endpointing":0.1,"maximum_duration_without_endpointing":15,
 "language_config":{"languages":["tr"],"code_switching":false},
 "messages_config":{"receive_partial_transcripts":true,"receive_final_transcripts":true,
                    "receive_speech_events":true,"receive_acknowledgments":false}}
```
(`receive_acknowledgments:false` and the 0.1 / 15 values are choices; 0.05-0.1 and 15 come from the "Voice Agents"
row of the recommended-parameters page.) For telephony pass-through: `encoding: "wav/ulaw"`, `bit_depth: 8`,
`sample_rate: 8000` (8000 is in the allowed list; that mu-law at 8000 Hz is the intended telephony pairing is not
spelled out: UNVERIFIED).

## Init response (`201 Created`)
Reference: `id` (string UUID, required), `created_at` (date-time, required), `url` (string URI, required; "WebSocket
URL, including a temporary session token").
```json
{
  "id": "45463597-20b7-4af7-b3b3-f5fb778203ab",
  "created_at": "2023-12-28T09:04:17.210Z",
  "url": "wss://api.gladia.io/v2/live?token=4a39145c-2844-4557-8f34-34883f7be7d9"
}
```
The quickstart example shows only `id` and `url`: parse `created_at` as optional.

## Client -> server messages
Audio can be sent two ways (quickstart / getting-started / AsyncAPI):
- **Binary frame**: raw audio bytes in the session's encoding/bit depth/sample rate/channels, no wrapper
  (`socket.send(buffer)`). This is what brisvox should use (no base64 overhead).
- **JSON text frame** with base64:
```json
{
  "type": "audio_chunk",
  "data": {
    "chunk": "<base64-encoded audio>"
  }
}
```
Chunk size: the support article says "small, continuous chunks (e.g. 20-50 ms of audio)"; the getting-started and
quickstart pages specify no recommended or maximum size.

Stop:
```json
{
  "type": "stop_recording"
}
```
Per the stop-recording page: "After reception, no more audio chunk will be accepted and post-processing will start."
(the page description also says the payload is meant to close the WebSocket connection; getting-started says
`stop_recording` tells Gladia to process the remaining audio and start post-processing.)
Alternative: `socket.close(1000)`; post-processing still runs in the background with results to the configured
callback.

There is **no** documented client message for: finalize/flush-without-ending, keepalive/ping, cancel/discard,
or mid-session reconfiguration (searched the AsyncAPI client messages: only `audio_chunk` JSON, binary audio,
`stop_recording`). Hence the Finalize mapping below.

## Server -> client messages (all JSON text frames, discriminated by `type`)
Common envelope: `session_id` (UUID), `created_at` (ISO 8601 UTC), `type`, and a `data` object (absent on
`start_session`, `start_recording`, `end_session`).
- `transcript` — `data.id` (utterance id), `data.is_final` (bool: "Indicates if the transcript is final or
  partial"), `data.utterance` {`start`, `end` (seconds from session start), `confidence` (0..1), `channel` (0-based
  int), `words[]` {`word`, `start`, `end`, `confidence`}, `text`, `language` (BCP-47)}. The AsyncAPI spec's only
  transcript example (a final):
```json
{"session_id":"550e8400-e29b-41d4-a716-446655440000","created_at":"2025-09-19T12:34:10Z","type":"transcript","data":{"id":"00-00000011","is_final":true,"utterance":{"start":0.0,"end":0.48,"confidence":0.91,"channel":0,"words":[{"word":"Hello","start":0.0,"end":0.35,"confidence":0.91},{"word":"world","start":0.36,"end":0.48,"confidence":0.91}],"text":"Hello world.","language":"en"}}}
```
  The quickstart/getting-started multi-channel example omits `is_final` and `words` (treat both as optional; a
  missing `is_final` must not be silently read as final: UNVERIFIED which default applies) and uses id format
  `"00_00000000"` (underscore) versus `"00-00000011"` (hyphen) in the spec: treat `id` as an opaque string:
```json
{
  "type": "transcript",
  "session_id": "de70f43f-3041-46e0-892c-8e7f53800a22",
  "created_at": "2025-04-09T08:44:16.471Z",
  "data": {
    "id": "00_00000000",
    "utterance": {
      "text": "Hello, I'm Sami. I'm the first speaker",
      "start": 0.188,
      "end": 2.852,
      "language": "en",
      "channel": 0
    }
  }
}
```
  No partial (`is_final:false`) example exists in any fetched page; shape is the same with `is_final:false`
  (per the partial-transcripts page: "The message contains a partial transcript, which is subject to change."), exact
  field subset for partials (e.g. `words`): UNVERIFIED.
- `speech_start` / `speech_end` (on by default via `receive_speech_events`; `data.time` seconds from session start,
  `data.channel` int):
```json
{"session_id":"550e8400-e29b-41d4-a716-446655440000","created_at":"2025-09-19T12:34:06Z","type":"speech_start","data":{"time":1.24,"channel":0}}
{"session_id":"550e8400-e29b-41d4-a716-446655440000","created_at":"2025-09-19T12:34:09Z","type":"speech_end","data":{"time":3.10,"channel":0}}
```
  The recommended-parameters page calls the end event `speech_stop` once; every schema page says `speech_end`:
  use `speech_end`, ignore unknown types.
- Acknowledgments (on by default; `receive_acknowledgments`), both carry `acknowledged` (bool) and `error`
  (object or null) and **reuse the client message name as `type`**:
  - `type:"audio_chunk"`: `acknowledged` ("True if the server successfully received and buffered the audio
    chunk"), `data.byte_range` [start,end] inclusive, `data.time_range` [start,end] seconds inclusive. Doc example
    values: byte_range `[0, 4095]`, time_range `[0, 0.256]`, acknowledged true, error null.
  - `type:"stop_recording"`: `acknowledged` true if accepted, false if rejected; `data` provider context, may be
    empty. Rejection example: `error` = `{"status_code":409,"exception":"ConflictError","message":"Recording
    already stopped"}`.
- Lifecycle (only with `receive_lifecycle_events:true`, default false): `start_session` ("emitted once when the
  session is created and ready"), `start_recording` ("when audio capture/streaming begins"), `end_recording`
  (`data` provider-defined, example `reason: "user_request"`, `received_total_bytes: 1048576`; "provider has
  stopped receiving audio"), `end_session` ("session is closed and no further data will be sent"; no `data`).
  Whether `end_session` is actually sent on the live socket as the last message before the close (versus
  callback/webhook only) is not stated: UNVERIFIED.
- Post-processing: `post_transcript` (`data.full_transcript`, `languages[]`, `sentences`, `subtitles`
  (`srt`/`vtt`/`ass`), `utterances`), `post_final_transcript` (`data.metadata` {`audio_duration`,
  `number_of_distinct_channels`, `billing_time`, `transcription_time`} + `transcription` and add-on results),
  `post_summarization` (`data.results` string). Example (spec):
```json
{"session_id":"550e8400-e29b-41d4-a716-446655440000","created_at":"2025-09-19T12:45:00Z","type":"post_final_transcript","data":{"metadata":{"audio_duration":123.45,"number_of_distinct_channels":1,"billing_time":120.0,"transcription_time":3.2},"transcription":{"full_transcript":"hello world"}}}
```
  (The `post_transcript` sub-fields `languages`, `sentences`, `subtitles`, `utterances` were not re-confirmed in the
  2026-10-08 fact-check: UNVERIFIED.) The quickstart does not document the post-processing message types; brisvox
  ignores them.
- Add-on messages (`translation`, `named_entity_recognition`, `sentiment_analysis`) only when enabled; unused.
- Error object used inside messages: `{"status_code": <int|string>, "exception": "<name>", "message": "<text>"}`
  (example `{"status_code":400,"exception":"InvalidArgument","message":"Unsupported sample rate"}`). A standalone
  top-level error message type for the live socket: not found in the fetched pages (`receive_errors` flag exists):
  UNVERIFIED.

## Result semantics (mapping to brisvox `SttEvent::Transcript`)
- Partials and the final of one utterance share `data.id`. "A newer partial replaces the previous provisional text
  for that utterance." "When `is_final` is true, the final text replaces the provisional text." "The resulting
  transcript contains one entry for that utterance—do not append each update as a separate line."
- `is_final:false` -> `Transcript{is_final:false,...}` replacing the in-progress text.
- `is_final:true` -> the utterance is fixed ("The message contains the final, most accurate transcript for an
  utterance. This transcript will not change."). Gladia utterances are closed by endpointing (silence >=
  `endpointing` seconds, or `maximum_duration_without_endpointing`), so one Gladia final == one utterance == one
  endpointed segment. Mapping chosen for brisvox: every final -> `is_final:true, speech_final:true,
  from_finalize:false`. That each final marks the end of the speaker's turn (not just a pause) is an inference from
  the endpointing page, not a doc statement.
  Because low `endpointing` (0.05-0.1 s) may split a sentence on a hesitation (endpointing page: "can split
  sentences if the speaker hesitates briefly"), the engine's own turn logic (VAD + Smart Turn) should decide when
  the turn is over; consecutive Gladia finals can be concatenated like Deepgram `is_final` segments.
- "Partial transcripts use a faster, smaller model than final transcripts" -> partial text can differ from the
  final; finals are more accurate (partial-transcripts page). Don't diff partial vs final.
- Partial accuracy drops with multiple languages / code switching ("Partial transcripts accuracy deteriorates when
  multiple languages and/or code switching are enabled."): use `languages:["tr"]` only.
- `finalized utterance != end of session`: finals arrive with the session still open.
- Empty-text partials/finals and how silence/noise is reported: UNVERIFIED.

## Finalize / flush / cancel semantics
- **No Finalize equivalent.** `stop_recording` ends the session (no more audio accepted, then post-processing, then
  the server closes with 1000 per the quickstart), so it cannot be used for a per-utterance flush on a long-lived
  session.
- The only latency lever is endpointing: the utterance closes automatically after `endpointing` seconds of
  silence. Docs: "Neither setting guarantees that a final transcript arrives within that many seconds" (transport and
  processing delay still apply).
- Proposed adapter mapping for `SttInput::Finalize` (adapter-side, not a Gladia feature):
  1. If an open utterance exists (a partial with no final yet), wait up to a `finalize_timeout` for Gladia's own
     final (low `endpointing` makes this quick), then deliver it with `from_finalize:true, speech_final:true`.
  2. On timeout, synthesise `Transcript{text: last partial, is_final:true, speech_final:true, from_finalize:true}`
     (same approach as the Deepgram adapter) and drop the later real final for that `data.id` as a duplicate.
  3. If no utterance is open (final already delivered), answer at once with an empty/synthetic
     `from_finalize` final so the engine's Finalize is never left unanswered.
  Alternative (not recommended): `stop_recording` + a new init per utterance (extra REST round trip each time,
  counts against the 30-session concurrency limit while post-processing runs: UNVERIFIED).
- Whether `stop_recording` flushes a pending (unfinalised) utterance into a last `is_final:true` transcript before
  the post-processing messages: UNVERIFIED (getting-started: tells Gladia to process the remaining audio).
- Cancel/discard buffered audio: none; drop the socket. After a hang-up (input dropped) send `stop_recording` or
  close 1000, then discard any results (brisvox contract: nothing follows a hang-up).

## Keepalive / timeouts
- No application-level ping/keepalive message is documented. Support article (possibly dated): "roughly 30 seconds
  of inactivity" (no audio sent) closes the socket with code **4408**; close code **4504** "after 1 minutes if
  not text is transribed, or no audio is sent" (the article does not say whether both conditions apply together).
  Neither code appears in the official reference (AsyncAPI defines no close codes). The article also advises
  enabling TCP keep-alive on long sessions.
  => Keep sending audio (even digital silence) continuously; do not rely on a pause. UNVERIFIED how the server
  treats silent audio vs the 4504 "no text transcribed" rule (a mostly silent call could hit 4504 even with audio).
- Max session duration 3 hours on all plans ("a single live session cannot exceed 3 hours and is terminated at that
  point"; start a new session before the limit).
- Socket close codes in the reference: `1000` = intentional close (e.g. after the session ended / post-processing
  done); "any other code: reconnect to the same URL". Reconnect: getting-started says you can reconnect to the same
  URL and resume where you left off; the support article says "the token remains valid" and to re-open the same
  session using the original URL. No retry intervals, backoff or max retries given; what is replayed/lost on resume
  (audio in flight, utterance state, timestamp base): UNVERIFIED.

## Timestamps / interim results
- `utterance.start/end` and `words[].start/end` are seconds from the **session start** (spec: "seconds from session
  start"). Whether they continue monotonically across a resumed socket: UNVERIFIED.
- `speech_start`/`speech_end` `data.time` are in the same time base ("Time in seconds from session start").
- `ack` `time_range` gives the audio time covered by each accepted chunk (debugging aid only).
- Latency guidance from the docs: lower `endpointing` suits voice agents; "Voice Agents" row of the recommended
  parameters page: `endpointing` "0.05 - 0.1", `maximum_duration_without_endpointing` 15, partials on, custom
  vocabulary on; "If utterances get cut off mid-sentence, raise `endpointing` slightly." No numeric latency figures
  appear in the docs. Vendor marketing (seen only as search snippets, not fetched pages: UNVERIFIED): partials "< 120
  ms" / "< 103 ms", "around 270 ms average". Measure with the sim/live test instead.

## Concurrency / rate limits
- Live concurrent sessions: Free 1, Paid 30, Enterprise on demand (concurrency page). Pre-recorded: 3 / 25 /
  on demand. Paid queue (300) is for pre-recorded only.
- HTTP 429 from init (and, per support article, opening a second socket on Free) when the concurrency limit is
  reached; 429 can also mean API rate limiting, "handle separately". Body format and retry header: the concurrency
  page says to honor "any retry header documented for the endpoint" but none is specified for live init.
  Docs advice: bounded retries with exponential backoff and jitter. Request-rate limit value: not given.
- Session cap 3 h; channels max 8.
- Docs recommend the official SDK for reconnection/continuity/buffering; brisvox must implement these itself.

## Turkish support
- Turkish is `tr`; supported by `solaria-1` (table row: Turkish | tr | Solaria-1 Yes | Solaria-3 No | auto-discovery
  and code-switch Yes | translation Yes). The page lists 100 languages and does not mention live/real-time, but live
  supports only `solaria-1`, which lists Turkish. Solaria-3 supports only English, French, German, Italian, Spanish.
- Use `language_config: {"languages":["tr"],"code_switching":false}` for monolingual Turkish (one language set = it
  is used for the transcription; with none or several languages the model auto-detects; the code-switching page says
  not to enable code switching with an empty list because the detector evaluates every utterance against 100+
  languages, causing misdetections especially between similar-sounding languages, and recommends a small set of
  expected languages for best accuracy and latency).
- Quality of live Turkish (partials vs finals, casing `I`/`İ`) is not documented: UNVERIFIED; check in the live test.
- Custom vocabulary works live (`realtime_processing.custom_vocabulary` + `custom_vocabulary_config`: `vocabulary`
  strings or `{value, pronunciations[], intensity, language}`, `default_intensity`; suggested intensity 0.4-0.6
  (guidance, not a schema limit); no term-count cap stated, long lists increase false positives).

## Pricing
- https://www.gladia.io/pricing (2026-10-08): Starter pay-as-you-go real-time $0.75/hr; Growth "as low as" $0.25/hr
  with upfront commitment; Enterprise custom (annual plan); sign-up gives EUR 50 one-time free credits (FAQ: roughly
  "60+ hours of real-time transcription"). "Prepaid wallet": UNVERIFIED (not seen on the re-fetched page).
  Multi-channel is billed duration x channels (getting-started page, not the pricing page). Recheck before quoting.

## Data retention
- The retention page does not separate pre-recorded from live; it gives one table: Free: audio, transcription, API
  metadata and logs 1 year; Paid (Starter/Growth/Enterprise): audio and transcription 3 weeks, API metadata 1 year,
  logs "limited period". Whether it applies identically to live sessions: UNVERIFIED. Enterprise can request custom
  retention (0 = deletion within 24 h) or zero data retention (ZDR; results only via callback).

## Gotchas / implementation notes
- Two-step connect: REST init then WebSocket; both need timeouts (engineering invariant, see CONTRIBUTING.md). Treat init 401/403-style as
  `Rejected{status}`; 429 as `Rejected{429}` with a bounded retry in the caller.
- The `url` contains the session token: redact in logs/errors (`Protocol` errors must not contain credentials).
- Interims are OFF by default (`receive_partial_transcripts` false).
- Ack messages reuse `type` values `audio_chunk` and `stop_recording`; disambiguate by presence of `acknowledged`
  (and absence of `data.chunk`), or switch acks off. Unknown `type`s must be ignored.
- Gladia final == endpointed utterance, not a flush response; Finalize needs adapter-side synthesis.
- Declared audio params must match what is sent; a mismatch makes the server mis-parse audio (error text
  `Unsupported sample rate` appears in the spec as an `InvalidArgument` example).
- Live sessions are stored (see retention). The index (llms.txt) lists "List transcriptions" and "Delete
  transcription" pages for live. Verified on https://docs.gladia.io/api-reference/v2/live/delete.md:
  `DELETE https://api.gladia.io/v2/live/{id}` with `x-gladia-key`; 202 = deleted, 401, 403 (job not in a deletable
  state), 404. The exact list route path: UNVERIFIED (not fetched).

## UNVERIFIED summary
1. Session token lifetime / delay allowed between init and socket open. 2. Whether `region` changes the returned
`url` host; default region. 3. Exact shape of partial transcripts (no example anywhere), default of a missing
`is_final`, empty-text messages. 4. Whether `stop_recording` flushes the pending utterance as a final transcript;
whether `end_session` appears on the socket. 5. Standalone error message type on the socket and the close codes
`4408`/`4504` (support article only, official reference defines none), plus 4504 behaviour with silent audio.
6. Timestamp continuity and what is replayed after a reconnect-and-resume; retry/backoff values. 7. `wav/ulaw` at
8000 Hz as the telephony pairing. 8. Latency numbers (vendor marketing only; no doc figures). 9. Live Turkish quality
and whether the retention table applies to live. 10. Effect of `audio_enhancer`, acks off and `speech_threshold` on
latency. 11. Max chunk size. 12. `code_switching` ignored with exactly one language; `Content-Type` marked required;
"prepaid wallet" on the pricing page; list-live route path; `post_transcript` sub-fields. (`DELETE /v2/live/{id}`
is now verified.)

## Gotchas found during implementation
Adapter: `brisvox_providers::gladia` (`GladiaStt`, `GladiaConfig`; feature `provider-gladia`); tests in
`crates/brisvox-providers/tests/gladia.rs` (loopback mock: HTTP init + WebSocket) and the unit tests of the module.
No live run was possible while writing (no key): everything below that depends on Gladia's real behaviour is marked.

- **Two-step connect under one deadline.** `connect()` = `POST /v2/live` (reqwest, shared rustls config, no proxy, no
  redirects) then the WebSocket on the returned `url`; both together are bounded by `io_timeout` on the injected clock
  (a stalled init or a stalled upgrade ends in `Core(Timeout)` at the deadline). The returned URL is a secret
  (`Secret<String>`, dropped after the handshake, never formatted into errors or logs); it must be `wss://` (`ws://`
  only on loopback hosts, for the mock). The init `id` equals the token in the quickstart example, so it is not logged
  either. Init errors log only the status and Gladia's `request_id`; the error body is not logged (it may echo fields).
- **Orphaned sessions are deleted (review fix).** `POST /v2/live` creates a session (one concurrency slot: 1 free /
  30 paid) before the WebSocket exists. If the upgrade then fails (refused, stalled past `io_timeout`, TLS error) or
  the connect future is dropped (caller cancelled, hang-up during a reconnect), the session would sit on its slot
  until Gladia expires it (undocumented), and on the free plan the retry would get 429. The adapter keeps the init
  response's `id` (only if it is 1-128 ASCII letters/digits/`-`/`_`, so it can never alter the request path; treated
  as a secret like the token, since the quickstart shows them equal) and a drop guard sends one detached
  `DELETE /v2/live/{id}` with `x-gladia-key`, bounded by `close_timeout`, answer ignored, failures logged without the
  URL. A refused init created nothing and sends nothing; a successful connect disarms the guard (the session is then
  retired through `stop_recording`, as before). Without a Tokio runtime (guard dropped outside one) nothing is sent.
  UNVERIFIED: that the DELETE frees the slot of a session that never streamed (the page lists 403 for a job not in a
  deletable state); without it the leak is no worse than before. Best effort also means the retry may still race the
  DELETE and see one 429, which the reconnect loop retries (408/429 are retryable, other 4xx are fatal).
- **`sample_rate_hz` is an assertion, not an override (review fix).** The adapter does not resample, so the rate
  declared at init (and the byte rate behind the replay ring, pacing, silence and the timeline) is always the
  `SttConfig` rate of the audio the caller sends. `GladiaConfig::sample_rate_hz = Some(hz)` is accepted only when `hz`
  equals that rate (and Gladia supports it); any other value fails `connect()` with a config `Protocol` error before
  any network call instead of declaring audio at a rate it is not in. The field is kept (the config struct is public
  API) rather than removed. Unsupported session rates (24 kHz) are still refused; the caller must resample.
- **Status mapping.** Any non-2xx init answer and any non-101 upgrade answer is `Rejected { status }` (401, 403, 422,
  429, 5xx...). Reconnecting stops at once on a 4xx other than 408/429 (credentials and request errors do not heal).
- **Finalize has no wire message.** Nothing is sent for `SttInput::Finalize`. The adapter answers it itself:
  (1) utterance open (a partial without a final, or `speech_start` with no final since; a final clears that, because
  the order of `speech_end` and the final is not documented, and so does giving up on a Finalize), or audio the server
  has not answered yet may hold speech (the replay ring is not empty and the session has not sent a single
  transcript/speech event yet, or the session is being replaced, or it was replaced and has not said anything yet):
  wait up to `finalize_timeout` (default 800 ms, counted from the request, also across a reconnect) for Gladia's own
  final of that utterance and deliver it as `from_finalize` + `speech_final`;
  (2) on timeout deliver the last partial (or empty text) as a synthetic `from_finalize` final and drop the real final
  of that utterance if it arrives later (delivering corrected text would duplicate words in the engine's transcript;
  same decision as for Deepgram except that Deepgram delivers a differing late text); (3) nothing open
  and nothing in flight: empty `from_finalize` final at once. The "in flight" rules exist because partials lag the
  audio: a Finalize sent right after the first audio of a session, or while the caller's audio is being replayed into a
  replacement session, must not be answered empty while the utterance is still on its way (the contract suite
  exercises the first; the reconnect cases have tests of their own). Cost: a Finalize on a call whose Gladia never
  produced any message (VAD fired on noise) waits `finalize_timeout` once; a Finalize on an idle call right after a
  reconnect (everything already final) does not wait. **Known limit (deliberate):** on a live session that has
  spoken before, a short utterance whose first message has not arrived when the Finalize comes is answered empty
  and its final arrives later as an ordinary endpointing final. Waiting for "audio since the last final" on every
  Finalize would put `finalize_timeout` on every turn (the engine sends one Finalize per VAD stop and keeps streaming
  audio after it, while the provider's own final normally arrives first), and the engine accepts a `speech_final`
  that answers no Finalize (stt stage docs).
- **Every Finalize is answered exactly once (review fix).** The engine's STT stage keeps one ledger entry per
  `Finalize` it sends and pops one per `from_finalize` + `speech_final` result; an unanswered entry makes it declare the
  session dead after its own 3 s timeout. A `Finalize` that arrives while another waits (a second VAD stop before the
  final, one sent while the session is being replaced, one on a session that has not heard anything yet) is therefore
  counted, not dropped. The first request owns the open utterance and the `finalize_timeout` deadline (counted from
  that first request); whatever answers it (the real final, a blank final, the timeout) is followed by one empty
  `from_finalize` + `speech_final` final per extra request. The count survives a reconnect and is capped at 32 (the
  engine's default `finalize_capacity`; a request beyond the cap is ignored, so a caller that respects the engine's
  bound loses nothing and the burst of empty answers stays bounded). Alternative considered: queueing the requests and
  timing each separately; rejected as more state for the same observable result, because the engine only counts
  answers.
- **Latency of the real answer** is `endpointing` (default here 0.1 s, the voice-agent value) plus Gladia's processing;
  the engine keeps streaming silence after the VAD stop, which is what lets Gladia's endpointing fire. Whether
  endpointing counts audio time or wall time: UNVERIFIED (matters only if the engine stops sending audio after VAD).
- **Partials/finals.** `is_final` missing is read as a partial (the docs' quickstart example omits it; reading it as a
  final could freeze text that is still provisional). Every real final is `speech_final: true`. Blank partials and
  blank finals are dropped unless a Finalize waits for them. Utterance ids are opaque and scoped to the session; after
  a reconnect the assembler forgets them (a final of any id then answers a pending Finalize).
- **Acknowledgments are opt-in** (`receive_acknowledgments` default false, as in the recommended body). Gladia sends
  nothing while nobody speaks, so with acks off an idle line is indistinguishable from a dead server and the adapter's
  response watchdog is OFF (that watchdog needs the acks). Dead links are caught independently of them by a WebSocket
  liveness ping (`liveness_ping`, default on): after `keepalive_interval` without any frame from the server the adapter
  sends a Ping and any frame within `io_timeout` proves the link (RFC 6455 requires a Pong); none counts as a lost
  session, replaced like any drop (`Core(Timeout)` when no reconnect is left). UNVERIFIED: Gladia's handling of pings
  is undocumented; if a live run shows healthy calls replaced every `keepalive_interval + io_timeout`, set
  `liveness_ping: false`. With `acknowledgments: true` every audio
  chunk is acked, and `io_timeout` without any message while audio flows is a lost session (replaced like any drop,
  `Core(Timeout)` when no reconnect is left); the silence keepalive is then at most `io_timeout / 2` so an idle caller
  stays alive. UNVERIFIED: whether Gladia acks **binary** frames the way it acks JSON `audio_chunk` messages; that is
  why acks are off by default. The first live run must check it (turn the flag on and watch for `Core(Timeout)`).
  `acknowledged: false` on an audio ack is fatal (`Protocol`, only the exception name and status in the text).
- **Keepalive** is audio: after `keepalive_interval` (default 5 s, must be < 30 s) without audio the adapter sends 20 ms
  of digital silence in the session encoding (PCM 0x00, mu-law 0xFF, A-law 0xD5). Generated silence shifts the session
  timeline, and audio the replay ring evicted before it could be sent leaves a gap in it; both are recorded as runs at
  the moment they happen (session position, stream position, length), and an utterance end time is mapped back to
  a stream position through those runs, so later silence or gaps never move the mapping of older audio. Replaying too
  much is not a safe error here (the words would come twice), so there is no "errs toward over-replay" slack.
- **Reconnect = new session, bounded replay.** Any unexpected close (also code 1000 mid-call, 4408/4504, a server
  `end_session`/`end_recording`, an `error` message with status >= 500) opens a **new** session with a new init call
  (default 1 attempt, backoff 100 ms doubling to 5 s) and replays the audio since the end of the last final (ring
  bounded by `replay_window`, 8 s) at `replay_speed` (default 2.0; Gladia documents no limit on how fast audio may be
  sent: UNVERIFIED, lower it if the live run shows throttling). The same-URL resume the docs mention is not used: the
  token lifetime and what a resumed socket replays are undocumented. Each attempt costs an init call and a concurrency
  slot (1 free / 30 paid), so the attempt budget is restored only by a transcript or ack, or by a session that lived
  30 s. After the attempts run out the error is the last connect error (e.g. `Rejected { status: 429 }`) if there was
  one, else `Disconnected` (or `Core(Timeout)` after silence). A session that is replaced or failed while its socket
  may still work (silence, a server error message, a protocol error; not after a failed write or a close from the
  peer) gets `stop_recording` and a close frame first, bounded by `close_timeout`, so that it frees its concurrency
  slot before the new init call (otherwise 429 on the free plan).
- **Hang-up** (the input or the events receiver dropped, also during a reconnect backoff or a stalled re-init, where
  no further init call is made) sends `stop_recording` and waits at most `close_timeout` for the server to close; events close first and
  nothing after the hang-up is delivered. Whether `stop_recording` flushes a pending utterance is irrelevant here.
- **Errors on the socket.** A `type: "error"` message is accepted in both plausible shapes (`error` or `data` carrying
  `status_code`/`exception`): status >= 500 reconnects, anything else is fatal `Protocol` (UNVERIFIED shape; the free
  text `message` is never copied into errors). Only a frame that is not a JSON object is fatal `Protocol`. Fields are
  read leniently because the partial shape is UNVERIFIED: `text: null` or a number reads as blank, a numeric `id` is
  kept as its decimal text, a non-boolean `is_final` is a partial, a `type` that is not a string or a transcript
  without a body is ignored. Unknown types, lifecycle, post-processing and add-on messages are ignored.
- **`warm()`** sends a credential-less `HEAD` to the init URL (DNS + TLS + pooled connection) and builds the shared TLS
  config; it never creates a session (concurrency slot, unknown token lifetime). The test records the HEAD requests
  and asserts no `x-gladia-key`, `Authorization` or `Cookie` header, no header value or target containing the key.
- **The key's two protections have tests.** The init client uses no system proxy and follows no redirects: reqwest
  strips only `Authorization`, `Cookie` and `WWW-Authenticate` on a cross-host redirect, not a custom header such as
  `x-gladia-key`, and a proxy from the environment would see the header. `redirecting_init_endpoint_is_not_followed`
  answers the init call with a `307` to a second listener and asserts `Rejected { status: 307 }` and zero requests at
  the second listener; `tests/gladia_proxy.rs` (its own test binary, because it sets `HTTP_PROXY`/`ALL_PROXY` for the
  process) asserts the init call reaches its target and not the proxy. Both were mutation-checked (removing the
  builder line fails them).
- **Init body** is built from typed structs: model `solaria-1`, `endpointing` 0.1, `maximum_duration_without_endpointing`
  15, `language_config` only when a language is known (`tr-TR` is sent as `tr`), partial/final/speech/error messages on,
  pre-/realtime-/post-processing events and lifecycle off (flags named in the reference; their effect on latency is
  UNVERIFIED), acks per the config, optional `custom_vocabulary`. Sample rates other than 8000/16000/32000/44100/48000
  are refused before any network call.
- **Tests.** The mock answers `POST /v2/live` (script per call: ok, status, garbage body, bad url, stall, hang,
  redirect), `HEAD` and `DELETE` (both recorded) and upgrades `GET`; reconnect tests cover the 401 (fatal after two
  inits) and the 408/429 (retried, session recovers after three inits) sides of the status split, and the
  Finalize-across-a-reconnect test drops the first session 400 ms after the request and stalls the re-init 300 ms, so
  a deadline restarted at the reconnect (about 1.6 s) cannot pass for the request-anchored one (about 0.9 s); the shared STT contract suite runs against it with acks on and no reconnect. The live test
  (`BRISVOX_LIVE_TESTS=1`, `GLADIA_API_KEY`) streams the first Turkish segment of `fixtures/tr_two_utterances.wav` in
  real time plus a second of silence, sends a Finalize and requires a non-empty Turkish final and an answered Finalize.

Fact-checked 2026-10-08: 60 claims checked, 8 corrected. Corrections: (1) data retention page covers the whole service
in one table (Free 1 year incl. logs; paid 3 weeks) and does not split pre-recorded/live; the old "describes only
pre-recorded retention" claim was wrong; (2) stop_recording quote replaced with the page's actual wording; (3)
final-transcript quote replaced with the page's wording ("final, most accurate transcript for an utterance. This
transcript will not change."); (4) `DELETE /v2/live/{id}` is documented (202/401/403/404), previously marked
UNVERIFIED; (5) multi-channel billing attributed to getting-started, not the pricing page, and "prepaid wallet"
downgraded to UNVERIFIED; (6) 401 meaning is "Missing or insufficient permissions"; (7) `code_switching` "ignored
with one language" downgraded to UNVERIFIED, and `Content-Type` "marked required" downgraded to UNVERIFIED; (8)
`callback_config` defaults listed fully (`receive_acknowledgments` and `receive_errors` false), plus token equals
session id in the quickstart example and `GET /v2/live/{id}` response details added.
