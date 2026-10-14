# AssemblyAI — STT (real-time streaming over WebSocket, v3 API)

Trait: `brisvox_providers::stt::SttProvider` (`SttInput::{Audio, Finalize}`,
`SttEvent::{Transcript{text,is_final,speech_final,from_finalize}, Error}`).

- Docs (all fetched 2026-10-08; everything below comes from these pages, via a fetch tool that summarises
  the page, so quoted strings are as reported by that summary):
  - Streaming WebSocket API reference: https://www.assemblyai.com/docs/api-reference/streaming-api/streaming-api
    (same content as https://www.assemblyai.com/docs/streaming/api-spec/streaming-websocket; the whole page was read
    in the 2026-10-08 fact-check, it ends with the `llmGatewayResponse` message and has no Error schema / close codes)
  - Streaming overview: https://www.assemblyai.com/docs/streaming
  - Message sequence: https://www.assemblyai.com/docs/streaming/message-sequence.md
  - Turn detection (Universal-3.x Pro): https://www.assemblyai.com/docs/streaming/turn-detection.md
  - Turn detection (Universal Streaming): https://www.assemblyai.com/docs/streaming/universal-streaming/turn-detection.md
  - Updating configuration mid-stream: https://www.assemblyai.com/docs/streaming/updating-configuration-mid-stream.md
  - Prompting and keyterms: https://www.assemblyai.com/docs/streaming/prompting-and-keyterms.md
  - Optimizing accuracy and latency: https://www.assemblyai.com/docs/streaming/getting-started/optimizing-accuracy-and-latency.md
  - Quickstart (raw WebSocket example): https://www.assemblyai.com/docs/streaming/getting-started/transcribe-streaming-audio.md
  - Stream a pre-recorded file in real time (pacing guide): https://www.assemblyai.com/docs/streaming/guides/stream_prerecorded_file_realtime.md
  - Benchmarks: https://www.assemblyai.com/docs/streaming/benchmarks
  - Model selection: https://www.assemblyai.com/docs/streaming/select-the-speech-model.md
  - Multilingual transcription: https://www.assemblyai.com/docs/streaming/multilingual-transcription.md
    (and the older path https://assemblyai.com/docs/streaming/universal-streaming/multilingual-transcription)
  - Language FAQ: https://assemblyai.com/docs/faq/language-support-for-real-time-transcription
  - Migration guide Universal Streaming -> Universal-3.6 Pro:
    https://www.assemblyai.com/docs/streaming/migration-guides/universal-to-universal-3-5-pro-streaming.md
  - Migration guide Deepgram -> AssemblyAI: https://www.assemblyai.com/docs/streaming/migration-guides/dg_to_aai_streaming
  - Common session errors and closures: https://www.assemblyai.com/docs/streaming/common-session-errors-and-closures.md
  - Rate limits: https://assemblyai.com/docs/streaming/rate-limits
  - Endpoints and data zones: https://www.assemblyai.com/docs/streaming/endpoints-and-data-zones.md
  - Temporary tokens: https://www.assemblyai.com/docs/streaming/authenticate-with-a-temporary-token.md
  - Models and pricing table: https://www.assemblyai.com/docs/getting-started/models and
    https://www.assemblyai.com/pricing ; billing: https://www.assemblyai.com/docs/billing-and-pricing.md
  - Doc index: https://assemblyai.com/docs/llms.txt

## Auth
- `Authorization: <ASSEMBLYAI_API_KEY>` header on the WebSocket upgrade, with NO `Bearer` prefix (reference, overview and
  quickstart all say so). Never put the permanent key in a URL.
- Alternative for browsers: `token=<temporary token>` query parameter. Token endpoint
  `https://streaming.assemblyai.com/v3/token` with the API key in `Authorization`; query params
  `expires_in_seconds` (required, 1-600) and `max_session_duration_seconds` (optional, 60-10800, default 10800);
  the JSON response carries a `token` field. HTTP method: the page text says POST but its examples and endpoint card
  use GET: UNVERIFIED. A token is NOT single-use (valid until it expires, can start several sessions). Usage is billed
  to the key that created it. brisvox is server-side, so the header is enough.
- Optional header `AssemblyAI-Version` pins the API version (default: latest). Value format: UNVERIFIED (the `Begin`
  example shows `"api_version": "2025-05-12"`).
- Failed auth closes the socket with 1008 (see Close codes).

## Endpoint(s) / regions
| Endpoint | URL | Processing |
|---|---|---|
| Edge routing (default) | `wss://streaming.assemblyai.com/v3/ws` | nearest region, any US or EU location |
| US data zone | `wss://streaming.us.assemblyai.com/v3/ws` | US regions only (us-east-1, us-east-2, us-west-1, us-west-2) |
| EU data zone | `wss://streaming.eu.assemblyai.com/v3/ws` | EU regions only (eu-central-1, eu-north-1, eu-south-1, eu-south-2, eu-west-1, eu-west-3) |
The v2 streaming API is retired (errors page: 410 "Deprecated endpoint"; use `/v3/ws`).

## Models (query `speech_model`)
| Model ID | Languages | Notes |
|---|---|---|
| `universal-3-6-pro` | 32 incl. Turkish (`tr`) | "default, recommended" for voice agents; docs call it "Universal-3.6 Pro Realtime"; sends `SpeechStarted`; no `format_turns` / `end_of_turn_confidence_threshold`; $0.45/hr |
| `universal-3-5-pro` | 18 (models page and FAQ) or 19 (select-model page), incl. Turkish per models page + FAQ | older Pro; $0.45/hr (models page); `mode`, `prompt`, `interruption_delay`, `previous_context_n_turns` are 3.5-Pro-specific per the reference |
| `universal-streaming-english` | English | `format_turns`, `end_of_turn_confidence_threshold`; $0.15/hr |
| `universal-streaming-multilingual` | en, es, de, fr, pt, it (NO Turkish) | same turn params as Universal Streaming; $0.15/hr |
Reference and select-model page say the default when `speech_model` is omitted is `universal-3-6-pro`, but the
migration guide says Universal Streaming "defaults to English" (i.e. the default may differ for old integrations):
always pass `speech_model` explicitly and check `Begin.configuration.model`. Unrecognised query parameters are
silently ignored, which is why the check matters.

## Query parameters (from the reference table; all optional)
| Param | Values / default |
|---|---|
| `speech_model` | see above; effective default `universal-3-6-pro` |
| `encoding` | `pcm_s16le` (default), `pcm_mulaw`, `opus`, `ogg_opus`, `aac` |
| `sample_rate` | integer 8000-96000, default 16000; ignored for `opus`, `ogg_opus`, `aac` |
| `language_codes` | list e.g. `["en","es"]`; steers (does not restrict) recognition; unset = native code switching; 3.6/3.5 Pro only; single element = monolingual session. URL serialisation (JSON array string like `keyterms_prompt`?): UNVERIFIED (docs show only a Python dict) |
| `language_detection` | bool, default false; adds `language_code`, `language_confidence` to turns (reference + select-model page: 3.6 Pro, 3.5 Pro and Universal Streaming Multilingual; the multilingual page lists only 3.6 Pro and Multilingual) |
| `format_turns` | bool, default false; Universal Streaming only (3.6 Pro always formats) |
| `end_of_turn_confidence_threshold` | float 0.0-1.0, default 0.4; Universal Streaming only (deprecated per migration guide). `1` = acoustic-only, `0` = silence-only (not recommended) |
| `min_turn_silence` | ms, clamped 50-10000. Defaults: Universal Streaming 400; 3.6/3.5 Pro by `mode` (see turn-detection page: min_latency 128, balanced 128, max_accuracy 512). The migration guide says 100 for U3.6 Pro and the LiveKit page says 100: the docs disagree, UNVERIFIED which is live |
| `max_turn_silence` | ms. Defaults: Universal Streaming 1280; Pro by `mode` (640 / 1280 / 2560 on the turn-detection page; 1000 per migration guide, 1536 per reference for 3.5 Pro): docs disagree |
| `mode` | `min_latency` / `balanced` / `max_accuracy` (reference says 3.5 Pro only, yet its `Begin` example shows model 3.6 Pro with `"mode": "balanced"`; the mid-stream page says "U3 Pro"; the optimizing page shows it with 3.6 Pro; treat 3.6 support as UNVERIFIED) |
| `vad_threshold` | float 0-1; Universal Streaming 0.4; Pro 0.2 (reference, turn-detection; 0.5 with `speaker_labels` per reference) or 0.3 (migration guide) |
| `interruption_delay` | 0-1000 ms, Pro only, server adds a fixed 256 ms (reference) |
| `include_partial_turns` | bool, default true (false when `redact_pii` on) |
| `keyterms_prompt` | list, max 100 terms, each max 50 chars (longer ignored), >100 -> error; raw-connection examples JSON-serialise it into a string (`json.dumps([...])`); percent-encoding not described. Included in price for 3.6 Pro and Multilingual; +$0.04/hr on Universal-Streaming English |
| `prompt` | string max 1750 chars (reference), context not instructions (Pro; reference says 3.5 Pro) |
| `agent_context` | string max 1750 chars; the agent's last TTS reply as context (3.6/3.5 Pro) |
| `session_heartbeat` | bool, default false; `Heartbeat` message every 5 s |
| `inactivity_timeout` | integer seconds 5-3600; no timeout if unset |
| `speaker_labels`, `max_speakers`, `voice_focus`, `redact_pii*`, `filter_profanity`, `domain`, `llm_gateway`, `previous_context_n_turns` (default 5, 0-100, 3.5 Pro) | not needed for brisvox v1 |
The reference marks most parameters `required: true` in its machine spec (and types them all as `string`) while the
intro says they are all optional: the spec is wrong or loose; treat all as optional.
Recommended brisvox set for Turkish (derived, not copied): `speech_model=universal-3-6-pro&sample_rate=16000
&encoding=pcm_s16le&language_codes=["tr"]` (serialisation UNVERIFIED) + `session_heartbeat` off. Telephony pass-through:
`encoding=pcm_mulaw&sample_rate=8000` (8000 for G.711 is not stated for this API: UNVERIFIED).

## Client -> server messages
Audio: BINARY frames of raw bytes (no JSON/base64). Control messages are TEXT frames (JSON):
```json
{ "type": "ForceEndpoint" }
{ "type": "Terminate" }
{ "type": "KeepAlive" }
{
  "type": "UpdateConfiguration",
  "prompt": "Transcribe product names accurately.",
  "keyterms_prompt": ["AssemblyAI", "Krabby Patty"],
  "min_turn_silence": 700,
  "max_turn_silence": 1600,
  "agent_context": "Sure — what date would you like to book?"
}
```
- Audio chunk rule: each chunk must be 50 ms to 1000 ms of audio (1600 bytes = 50 ms at 16 kHz 16-bit mono); a chunk
  outside that range closes the session with 3007. A text frame that is not valid JSON closes the session (3006).
- Pace sends at real time (the pre-recorded-file streaming guide: send "at the same pace it was recorded"; faster
  causes "degraded transcription accuracy, unexpected session closures, or other errors"). Faster-than-real-time input
  is listed as a 3007 cause ("Audio Transmission Rate Exceeded: Received <time> sec. audio in <time> sec"). An earlier
  revision of this file claimed a ~1.25x throttle and a 5-minute buffer limit: NOT found in any official page
  re-fetched on 2026-10-08, so UNVERIFIED (do not rely on it).
- `opus`: exactly one Opus packet per binary frame; `ogg_opus`: arbitrary chunks of the Ogg stream; `aac`: ADTS
  framing only (ADTS: UNVERIFIED, the reference says only "arbitrary chunks"). brisvox sends PCM, so these do not matter.
- `UpdateConfiguration` is a delta (omitted fields unchanged). Updatable: `prompt`, `keyterms_prompt` (replaces list;
  `[]` clears: UNVERIFIED), `min_turn_silence`, `max_turn_silence`, `end_of_turn_confidence_threshold` (Universal
  Streaming only), `vad_threshold`, `interruption_delay`, `mode`, `language_codes` (empty list clears steering;
  effective next turn), `session_heartbeat` (reference only), `agent_context` (each update replaces the previous:
  UNVERIFIED). Model support differs (mid-stream page: `mode`/`prompt`/`agent_context`/`interruption_delay`/
  `language_codes` are "U3 Pro" only). No acknowledgment message or error behaviour for invalid values is
  documented: UNVERIFIED.
- `KeepAlive`: resets the `inactivity_timeout` timer; only needed when `inactivity_timeout` is set and no audio is flowing.

## Server -> client messages (JSON text frames, discriminated by `type`)
`Begin` (once, right after connect):
```json
{
  "type": "Begin",
  "id": "3207b601-2054-48df-ba77-8784dfcf9fb8",
  "expires_at": 1772570132,
  "configuration": {
    "model": "universal-3-6-pro", "mode": "balanced", "api_version": "2025-05-12",
    "speaker_labels": false, "redact_pii": false, "filter_profanity": false,
    "domain": null, "voice_focus": null
  }
}
```
`SpeechStarted` (message-sequence page: Universal-3.6 Pro only, Universal Streaming does not send it; whether 3.5 Pro
sends it: UNVERIFIED; once per turn, immediately before the first `Turn`; only after the model produced an actual
transcript, so noise alone does not trigger it; `confidence` = average word confidence of the initial transcript):
```json
{ "type": "SpeechStarted", "timestamp": 1216, "confidence": 0.98 }
```
`Turn` (reference example; the reference's own example omits `word_is_final`, which the schema says is required):
```json
{
  "type": "Turn",
  "turn_order": 0,
  "turn_is_formatted": true,
  "end_of_turn": true,
  "transcript": "Hello world.",
  "end_of_turn_confidence": 1,
  "words": [
    { "text": "Hello", "start": 0, "end": 500, "confidence": 0.99 },
    { "text": "world.", "start": 500, "end": 1000, "confidence": 0.98 }
  ]
}
```
Partial (turn-detection page; note it shows no `words`):
```json
{ "type": "Turn", "turn_order": 0, "end_of_turn": false, "turn_is_formatted": false,
  "transcript": "my number is five five five", "end_of_turn_confidence": 0.0 }
```
Required `Turn` fields (reference schema): `type`, `turn_order` (int), `turn_is_formatted`, `end_of_turn`, `transcript`,
`end_of_turn_confidence`, `words`. Other `Turn` fields: `utterance` (only on `end_of_turn` messages); `language_code` +
`language_confidence` (only with language detection and a final turn; automatic on U3.6 Pro per the migration guide);
speaker fields only with `speaker_labels`. Each word: `text`, `start`, `end` (milliseconds), `confidence`, and a required
boolean `word_is_final` ("Whether the word is final"); `speaker`/`speaker_confidence` optional. Finer semantics of
`word_is_final` (e.g. whether partial turns carry non-final words): UNVERIFIED.
`Heartbeat`: `{"type":"Heartbeat","total_audio_received_ms":45000,"total_duration_ms":45205,"realtime_factor":0.9964,
"max_speech_probability":0.999954}` (`realtime_factor` near 1.0 = sending in real time).
`SpeakerRevision`: only with `speaker_labels`; ignore.
`Termination` (always the last message; nothing follows; server then closes with code `1000`; `session_duration_seconds`
is the billed duration):
```json
{ "type": "Termination", "audio_duration_seconds": 45, "session_duration_seconds": 47 }
```
`Error` (text frame sent just before the socket closes, per the message-sequence and errors pages). Its field schema
(`error_code`, `error`) is NOT in the reference text re-fetched on 2026-10-08: field names UNVERIFIED, no JSON example.
The close reason string is limited to 123 bytes, so longer errors carry the reason "See Error message for details"
(errors page, verified).
`LLMGatewayResponse` (`type` constant; per finalized turn, only when `llm_gateway` is configured; fields `turn_order`,
`transcript`, `data`): ignore.

## Turn semantics (mapping to brisvox `SttEvent::Transcript`)
- Each turn = one `SpeechStarted` (3.6 Pro), one or more partial `Turn`s (`end_of_turn:false`), one final `Turn`
  (`end_of_turn:true`). Every `Turn` re-transcribes the whole turn and supersedes the previous one of the same
  `turn_order`: REPLACE, do not append. `turn_order` increases monotonically; all messages of a turn arrive before
  the next turn's first message (the "before the next turn" ordering is from the earlier revision, UNVERIFIED).
- Pro (U3.6 / U3.5): the final `Turn` has `end_of_turn:true`, is fully formatted (`turn_is_formatted:true`),
  `end_of_turn_confidence` 1.0. Partials have `end_of_turn:false` (3.5 Pro: `turn_is_formatted` always equals
  `end_of_turn`). Partials come early (timed by `interruption_delay`), on a pause when the turn reads incomplete, and
  roughly every second during continuous speech (partial cadence: UNVERIFIED in the 2026-10-08 re-check). Detection is
  punctuation/semantic based: at `min_turn_silence` a terminal mark (`. ? !`) ends the turn; `max_turn_silence`
  forces the end.
- Universal Streaming: with `format_turns=true` you get an UNFORMATTED final (`end_of_turn:true`,
  `turn_is_formatted:false`) followed by a FORMATTED final for the same `turn_order` (confirmed by message-sequence
  page; the earlier quoted sentence "Treating the first as complete would process the turn twice" was not re-found
  verbatim: UNVERIFIED wording). `turn_is_formatted` is true on all turns, partials included, per the migration
  guide (conflicts with the previous sentence for the no-`format_turns` case): UNVERIFIED. A turn is complete only
  when `end_of_turn:true` AND `turn_is_formatted:true` (message-sequence page: "both `end_of_turn` and
  `turn_is_formatted` are `true`", written for Universal Streaming + `format_turns`). With `format_turns=false` the
  final has `turn_is_formatted:false` and there is no second message (message-sequence page, confirmed).
- brisvox mapping proposal (derived): partial -> `Transcript{is_final:false}`; final turn -> one
  `Transcript{is_final:true, speech_final:true}` (an AssemblyAI turn is the utterance, so each final turn ends the
  utterance); a final produced after a `ForceEndpoint` sets `from_finalize:true`. Concerning `from_finalize`:
  there is no marker on the wire (no equivalent of Deepgram's `from_finalize`), so the adapter must itself remember
  that a ForceEndpoint is outstanding and flag the next `end_of_turn` Turn (UNVERIFIED which final answers it when a
  natural endpoint fires at the same moment).

## Finalize / flush / cancel semantics
- `ForceEndpoint` = brisvox `Finalize`. Docs: "ends the current turn immediately, without waiting for silence";
  "without waiting for silence or punctuation"; the server responds with the `end_of_turn: true` message(s) for that
  turn immediately (turn-detection and message-sequence pages). Intended for "your own VAD or turn detection". It is
  documented on both the Universal Streaming and the U3.x Pro turn-detection pages.
  UNVERIFIED: behaviour when no turn is open / no speech heard since the last final (no answer? an empty-transcript
  final?); whether the audio still buffered in the server is included in the final; whether the final is formatted on
  Universal Streaming (message-sequence says "message(s)", i.e. possibly the two-message form). The adapter must
  time-box the wait (like the Deepgram adapter) and not assume an answer.
- `Terminate` = hang-up/finish. After it the server flushes in-flight messages before `Termination`: the final
  formatted `Turn` for audio already sent; on Universal Streaming a closing `Turn` with empty `transcript` for an
  unfinished turn. "Closing the socket early discards your last transcript" (message-sequence page). Keep reading
  until `Termination`. Always send it: billing is by connection time and unterminated sessions auto-close at 3 h and
  are billed in full. Since the brisvox contract says nothing follows a hang-up, drain and discard after `Terminate`
  with a timeout (like `close_timeout` in the Deepgram adapter).
- No "cancel/discard buffered audio" message. Discarding = close the socket and reconnect.
- Mid-stream tuning (`UpdateConfiguration`) is the alternative to reconnecting for turn-silence changes.

## Keepalive / timeouts
- The quickstart page documents no ping/pong or keepalive requirement (silence on the topic, not a guarantee).
- There is NO idle timeout by default (`inactivity_timeout` "no timeout if unset"). If set (5-3600 s) and nothing is
  received for that long, the session closes with 3006 "Session terminated due to inactivity: No messages received for
  <n> seconds"; `KeepAlive` messages prevent it. Default brisvox plan: leave `inactivity_timeout` unset, but then an
  idle open socket is still BILLED (session-duration billing): keep the connection only for the call.
- Whether the server drops a connection that never receives any audio: UNVERIFIED (not documented).
- Max session length 3 hours (close 3008 "Session Expired: Maximum session duration exceeded", cause "3 hours or the
  maximum set by a temporary token"). The earlier claim that 3008 is "delivered after the current turn ends or within
  60 s of the limit" and that extra time is billed was not re-found: UNVERIFIED. Temporary tokens can set a shorter
  limit via `max_session_duration_seconds`. `Begin.expires_at` is a Unix time (presumably that limit: UNVERIFIED).
  Calls longer than 3 h need a planned reconnect before `expires_at`.
- `session_heartbeat=true` gives a server message every 5 s of wall-clock time: useful as liveness for a client
  watchdog (the Deepgram adapter's `io_timeout` pattern), at the cost of a few bytes. Whether Heartbeats continue
  while no audio flows: UNVERIFIED.

## Timestamps / interim results
- Word `start`/`end` and `SpeechStarted.timestamp` are milliseconds. `SpeechStarted.timestamp` is "ms from the start
  of the stream". Whether word times are relative to the connection start (they restart on a new connection):
  presumed, UNVERIFIED.
- Latency: https://www.assemblyai.com/docs/streaming/benchmarks (fetched 2026-10-08) has no figures for 3.6 Pro
  ("not yet available") and nothing for Turkish; for Universal-3.5 Pro Streaming it gives Time to Complete Turn (end of
  speaker turn to finalized transcript) P50 568 ms, P90 829 ms. Its multilingual WER table lists only de, en, es, fr,
  it, pt. `mode=min_latency` is "Returns text as quickly as possible"; `balanced` is the default mode and the
  optimizing page presents it as aimed at voice agents.
- Billing counts connection time, not audio, so a silent connection costs the same.

## Concurrency / rate limits
- No hard cap on total open streaming sessions for paid accounts; the limit is NEW sessions per minute: Free starts
  at 5/min, Paid at 100+/min. Auto-scaling: at >= 70% use the next minute's limit grows by 10%; 50-69% unchanged;
  below 50% it drifts back to the account's starting value (so after quiet periods you may not open many at once).
  Custom limits via sales/support (stated as no extra cost: UNVERIFIED in the 2026-10-08 re-check). Limits are per
  account (all API keys share them).
- Exceeding it: the rate-limits page says close code 1008 with "Unauthorized connection: Too many concurrent sessions";
  the errors page lists the same text under code 3009. Docs conflict: handle BOTH 1008 and 3009 with that reason as
  "rate limited, back off" (UNVERIFIED which one the server sends). Sessions that are not closed properly still count.
- HTTP 429 for streaming: not documented.
- Default $50 of free credit for new accounts (billing page; credits do not expire).

## Close codes (errors page, v3)
| Code | Reason | Cause / handling |
|---|---|---|
| 1000 | (normal) | after `Termination` (message-sequence page) |
| 3005 | "Session Cancelled: An error occurred" | unknown server error; also the catch-all |
| 3006 | "Invalid Message Type: <message>" / "Invalid JSON: <json>" / "Invalid Message: <message>" (e.g. `[]`) / "Session terminated due to inactivity: No messages received for <n> seconds" | adapter bug or `inactivity_timeout` |
| 3007 | "Input duration violation: <time> ms. Expected between 50 and 1000 ms" / "Audio Transmission Rate Exceeded: Received <time> sec. audio in <time> sec" | chunk size outside 50-1000 ms, or sending faster than real time |
| 3008 | "Session Expired: Maximum session duration exceeded" | 3 h or token limit; reconnect |
| 3009 | "Unauthorized Connection: Too many concurrent sessions" | rate limit exceeded (see above) |
| 1008 | "Unauthorized Connection: Missing Authorization header" | key missing/invalid |
| 1008 | "Unauthorized Connection: <reason>" | account issue e.g. insufficient balance or disabled account |
| 1011 | "Internal error" | unexpected server-side error while connecting (e.g. in auth); "Retry. If it persists, contact support" |
| 410 | "Deprecated endpoint" | v2 retired |
An `Error` text frame precedes the close for error terminations; known closures such as 3008 do not trigger SDK
`on_error` (UNVERIFIED in the 2026-10-08 re-check). The old real-time API reference lists 4xxx codes (4001, 4008,
4029): those belong to the retired v2 API; do not use them. Retry guidance in the docs is only "retry" for 1011;
retry/backoff values elsewhere are UNVERIFIED (use bounded exponential backoff with jitter for 1011/3005/3009/1008-rate).
Whether a failed upgrade is reported as an HTTP status or as a close after 101: UNVERIFIED (1008 and 1011 are listed as
WebSocket close codes).

## Reconnect
- A new WebSocket is a new session with a new `Begin.id`, new `turn_order` numbering (presumed) and timestamps from
  zero (presumed): UNVERIFIED, no reconnect/resume guide was found. Interim hypotheses and any un-ended turn are lost.
- Audio sent faster than real time can close the session with 3007; the tolerated factor is UNVERIFIED (no official
  figure; the earlier "1.25x / 5 min buffered" numbers were not found in the docs), so pace a replay of buffered audio
  after a reconnect at ~real time (same shape as the Deepgram replay ring). Replay chunks must
  respect the 50-1000 ms rule (re-chunk a replay buffer; a short tail chunk below 50 ms would trigger 3007).
- Config mid-stream changes (`UpdateConfiguration`, keyterms) are lost on reconnect: re-apply via the query string.

## Turkish
- Turkish (`tr`) is supported in streaming ONLY by the Pro models: `universal-3-6-pro` (32-language list on the
  multilingual page, Turkish confirmed "Yes" there; the earlier enumerated list: af, ar, yue, ca, da, nl, en, et, fi,
  fr, gl, de, he, hi, it, ja, ko, zh, mr, no, nn, fa, pt, ro, ru, es, sv, tr, ur, vi, xh, zu was not re-verified item
  by item). `universal-3-5-pro` also lists Turkish (models page, language FAQ). `universal-streaming-multilingual` (en,
  es, de, fr, pt, it) and `universal-streaming-english` do NOT.
- Caveats: the select-model page itself does not list Turkish (it only links to the full list); the models page and
  FAQ say 3.5 Pro has 18 languages while the select-model page says 19; the multilingual page says 3.6 adds 13
  languages to the 3.5 Pro set and Turkish is not among the 13. The multilingual page lists no per-language accuracy
  figures or Turkish-specific limitations. The reference says 3.5 Pro excludes af, yue, et, gl, ko, mr, nn, fa, ro,
  ru, ur, xh, zu (13 codes; Turkish is not excluded).
- Language steering: `language_codes: ["tr"]` for a monolingual Turkish session (the page says single element is
  "intended for monolingual sessions"; other effects not described). Without it the model code-switches freely.
- Turkish accuracy/latency numbers: UNVERIFIED (none in fetched pages; the benchmarks page covers no Turkish). Plan a
  live comparison against Deepgram Nova-3 `tr` before choosing a default.

## Pricing
- Streaming is billed per hour of SESSION DURATION (time the WebSocket is open, idle time included).
- https://www.assemblyai.com/pricing (2026-10-08): Universal-3.6 Pro Realtime (`universal-3-6-pro`) $0.45/hr;
  Universal-Streaming English $0.15/hr; Universal-Streaming Multilingual $0.15/hr. Add-ons for the whole session:
  Speaker Diarization +$0.12/hr, Medical Mode +$0.15/hr, Voice Focus +$0.10/hr (U3.6 Pro only), Keyterms Prompting
  included on U3.6 Pro / Multilingual and +$0.04/hr on Universal-Streaming English. Volume discounts: UNVERIFIED in
  the 2026-10-08 re-check. Recheck before quoting.

## Gotchas / implementation notes
- Replace, don't append, within a `turn_order`. Treat `end_of_turn:true` as the utterance end on Pro; on Universal
  Streaming + `format_turns=true`, wait for `turn_is_formatted:true` on the final (avoid double-processing). For
  Turkish brisvox uses Pro, where this does not apply.
- Always read until `Termination` after `Terminate`; closing earlier loses the last transcript.
- Pro models: use `ForceEndpoint` from brisvox's own VAD/Smart Turn; the provider's own endpointing
  (`min_turn_silence`/`max_turn_silence`) is a fallback. For a pure-ForceEndpoint setup the docs only describe
  `end_of_turn_confidence_threshold=1` (Universal Streaming); no equivalent for Pro is documented (raising
  `max_turn_silence` is the closest knob): UNVERIFIED.
- Chunking: 50-1000 ms per binary frame. brisvox frames of 20 ms must be batched (>= 50 ms) before sending. The
  quickstart page gives no chunk recommendation (its example sends 4096-byte reads); the file-streaming guide defaults
  to 100 ms chunks (50 ms = closest to live mic) and the Deepgram migration guide uses 800 frames = 50 ms at 16 kHz.
- Control messages are text frames; audio binary. Never log the `Authorization` header or `token`.
- The reference's `llm_gateway` and speaker/redaction params are irrelevant for v1; unknown params are ignored
  silently, so typos fail silently (verify `Begin.configuration`).
- The docs describe the model naming as moving (3.5 -> 3.6 in the pages fetched); re-check the model list before
  coding, and pin `speech_model` explicitly.

## UNVERIFIED summary
1. Default `speech_model` when omitted (reference + select-model page say U3.6 Pro; migration guide says Universal
defaults to English).
2. `language_codes` URL serialisation (JSON array string vs repeated key). 3. Effective U3.6 Pro defaults for
`min_turn_silence`/`max_turn_silence`/`vad_threshold` (turn-detection page, migration guide and reference disagree).
4. `ForceEndpoint` behaviour with no open turn / no buffered speech, and what marks its answer (no `from_finalize`
analogue). 5. Whether a formatted second message follows a ForceEndpoint final on Universal Streaming.
6. Whether `mode` works on U3.6 Pro (reference: 3.5 Pro only, but its Begin example and other pages show it with 3.6).
7. Idle behaviour of a connection that never receives audio. 8. Whether `Heartbeat` is sent while no audio flows.
9. Rate-limit close code (1008 vs 3009) and whether upgrade failures come as HTTP status. 10. Reconnect/resume
semantics, turn_order/timestamp reset, retry/backoff values. 11. `pcm_mulaw` at 8000 Hz (rate guidance absent).
12. Temporary-token HTTP method (POST vs GET). 13. `AssemblyAI-Version` value format. 14. `Error` message field
names and JSON example. 15. `UpdateConfiguration` acknowledgement and error behaviour. 16. Turkish accuracy and
latency numbers; Turkish count discrepancy between 3.5 Pro pages (18 vs 19). 17. Fine semantics of
`Turn.words[].word_is_final`. 18. 3.6 Pro streaming latency figures (benchmarks page has none; only 3.5 Pro P50 568 /
P90 829 ms). 19. (resolved 2026-10-08: whole reference page read; no Error schema in it.) 20. Turn semantics for
Universal Streaming `turn_is_formatted` without `format_turns` (pages conflict). 21. The ~1.25x throttle / 5-minute
buffer claims (no official source found). 22. Whether 3.5 Pro sends `SpeechStarted`. 23. `UpdateConfiguration`
`keyterms_prompt: []` clearing the list; `agent_context` replace semantics. 24. 3008 delivery timing and extra-time
billing. 25. `Begin.expires_at` equals the session limit. 26. "no extra cost" for custom rate limits; volume discounts.

## Adapter (`brisvox_providers::assemblyai`, feature `provider-assemblyai`)
Code: `crates/brisvox-providers/src/assemblyai/` (`config`, `connection`, `protocol`, `assembler`, `batch`,
`replay`, `session`); tests: `crates/brisvox-providers/tests/assemblyai.rs` (local mock of the v3 protocol, the
shared contract suite, one live test). Public: `AssemblyAiStt`, `AssemblyAiConfig`, `Encoding`, `Region`.
- Auth: `Authorization: <key>` (sensitive header, no `Bearer`); key is a `Secret<String>`. `speech_model` is always sent
  (default `universal-3-6-pro`), `Begin.configuration.model` is compared with it (warning on mismatch).
- Connect waits for `Begin` under the same `io_timeout` as the handshake. A close frame instead of `Begin` maps by code:
  1008 -> `Rejected{1008}`, 1008 with "too many" or 3009 -> `Rejected{429}` (UNVERIFIED 9: both are handled), 3006/3007 ->
  `Protocol`, anything else -> `Rejected{code}`. Both ways a failed auth can arrive (HTTP status before the upgrade, close
  after it; UNVERIFIED) therefore end up as a `connect` error.
- Query: `speech_model`, `encoding`, `sample_rate`, `language_codes` (JSON array string, e.g. `["tr"]`, bare primary subtag
  of the session language; serialisation UNVERIFIED 2, `extra_params` is the escape hatch), `format_turns` (only for
  `universal-streaming*` models), `min_turn_silence`, `max_turn_silence`, `keyterms_prompt` (JSON array string),
  `session_heartbeat=true` (default), `inactivity_timeout` (opt-in), plus `extra_params`.
- Audio: 50 ms minimum per chunk (3007 otherwise). The engine's 10-20 ms frames are held until 50 ms are there (copy into a
  small buffer; a caller chunk already >= 50 ms is passed on uncopied); anything over 500 ms is split (never leaving a
  piece < 50 ms); whole samples only, and the 50 ms minimum is rounded UP to whole samples (at 22.05 kHz 50 ms is 1102.5 samples:
  2206 bytes of PCM16, never the 2204 that is 49.98 ms). A sub-50 ms remainder is held until the next audio; it is padded
  with silence to 50 ms only immediately before a `ForceEndpoint` (or after the opt-in `batch_flush`, default off, at least
  50 ms): padding on a short timer put fake silence mid-word under network jitter and sent audio ahead of real time (a 3007
  cause). Cost: the first frame of a burst waits for the next one; <= 49 ms of fabricated silence per `Finalize`. The replay ring stores these chunks and drops whole chunks only, so a replay
  is made of valid chunks too.
- Turns: partial `Turn` -> `Transcript{is_final:false}` (replace semantics; blank/repeated partials not forwarded); final
  `Turn` (`end_of_turn`) -> one `Transcript{is_final:true, speech_final:true}`. `from_finalize` is set on the first final
  after a `ForceEndpoint` (nothing on the wire marks it; UNVERIFIED 4). With `format_turns` on a Universal Streaming model
  the unformatted final is shown as an interim and the formatted twin is the final. Repeated finals of an ended
  `turn_order`, stale partials, and the empty closing `Turn` after `Terminate` are dropped. `SpeechStarted`, `Heartbeat`,
  `SpeakerRevision`, `LLMGatewayResponse` and unknown types are ignored.
- Finalize: `ForceEndpoint` after flushing the held audio. Bounded wait: `finalize_timeout` (800 ms) while a turn is in
  progress OR audio was given since the last final / `Finalize` (a short utterance such as "evet" can end by the engine's
  VAD before the model produced its first partial; the benchmark for completing a turn is P50 568 ms), `finalize_idle_timeout`
  (200 ms) when nothing is open: no audio since the last final / `Finalize`, OR the last server event was a final turn with no
  partial after it (the engine forwards silence too, so "audio since the final" is true again one frame later; a Pro model that
  ended "Evet." by itself and a VAD stop that comes later would otherwise wait the full 800 ms for a `ForceEndpoint` reply that
  is UNVERIFIED for a line with no open turn). Cost: a short utterance with no partial that follows a natural final gets the
  empty synthetic final after 200 ms, its real final arriving later is delivered as a late utterance (see below). The server
  may stay silent or send an empty final: both are handled. Then a synthetic final (last interim or empty, `speech_final`, `from_finalize`). The server's late real final is
  dropped when its text equals the synthetic one (case/punctuation-insensitive, Turkish-aware), else delivered as another
  utterance end; this only holds for 3 s after the synthetic final, later finals are ordinary utterance ends without
  `from_finalize`. A final that arrives while a `Finalize` waits answers it (it may be a natural endpoint that raced the
  `ForceEndpoint`).
- Keepalive / liveness: no idle timeout exists unless `inactivity_timeout` is set; then `{"type":"KeepAlive"}` is sent after
  `keepalive_interval` without audio. The response watchdog (`io_timeout` of server silence after audio was given) relies on
  `Heartbeat` messages (`session_heartbeat=true`, 5 s) because the server says nothing during silence; with `heartbeat` off
  a WebSocket `Ping` probes after `io_timeout` and a second `io_timeout` without any frame declares the connection lost
  (a Pong is standard WebSocket behaviour, AssemblyAI answering it is UNVERIFIED). The watch is paused when the line has been quiet for `keepalive_interval` (Heartbeats while no audio flows:
  UNVERIFIED 8). A silent connection is replaced like any lost one (`Core(Timeout)` when no reconnect is left). Tests cover the production ordering (`keepalive_interval` 4 s < `io_timeout` 10 s, scaled down): a half-open server is replaced at
  `io_timeout` while audio flows, and a single burst followed by a quiet line is not (until audio resumes). The timing defaults quoted in this file
  are asserted by a unit test in `config.rs`; the integration tests override them.
- Reconnect: drops (close 1000/1011/3005/3008/none, I/O error, unrequested `Termination`) open a new session (default 1
  attempt, backoff on the injected clock) and replay the audio the server has not finished: the ring is trimmed to the end
  of the last word of every final `Turn` (word times assumed to be ms from connection start: UNVERIFIED 10) and to the
  `ForceEndpoint` position when it is answered or given up on; bounded by `replay_window` (8 s). Replay is paced at
  `replay_speed` (default 1.0 = real time, as the doc says: faster than real time is a documented 3007 cause and the tolerated
  factor is UNVERIFIED 21; 1.0 never catches up while live audio keeps arriving, but cannot kill the call: a test feeds live
  audio during a paced replay against a mock that enforces the rate). Because a backlog never drains at 1.0, the replay does
  not resend what cannot matter: it starts 2 s before the first partial of the open turn (or before a pending `Finalize`'s
  `ForceEndpoint` position), else 1 s before the old connection's send position (design decision, conservative: the model
  reports a partial within a few hundred ms of speech); audio the old connection never received (written after its last send,
  or left behind by a lagging replay) is always replayed. Without this, 8 s of pre-speech silence in the ring made every later
  word and `ForceEndpoint` reach the server 8 s late for the rest of the connection. If the ring evicts audio that a lagging
  replay had not sent yet, the connection's time base is moved past the gap (the server's clock does not run over it), so word
  times still map to the right ring position and a delivered turn is not replayed. A wordless final
  (the doc shows turns without `words`, UNVERIFIED 10/17) trims the ring to the `ForceEndpoint` position it answered or to
  the audio sent so far, so a delivered turn is not replayed and delivered twice. A pending
  `ForceEndpoint` is re-sent after the replay. 1008 and 401-403 are never retried. The attempt budget is restored by a `Turn`
  or a `Heartbeat`.
- Close codes mid-session: 3006 whose reason says "inactivity" and 3007 whose reason says "Transmission Rate" are transient
  (reconnect with a re-paced replay); every other 3006/3007 is `Protocol` (an adapter bug) and ends the call.
- Terminate: besides hang-up, a best-effort `Terminate` (<= 250 ms) goes out before a silent connection is replaced and
  after a fatal error is delivered, because billing is by connection time and zombie sessions count against the
  concurrent-session limit.
- Hang-up: events close at once, `Terminate` is sent and the socket drained until `Termination`/close for at most
  `close_timeout`; nothing produced after the hang-up reaches the caller.
- Not done: planned reconnect before the 3 h session limit (`Begin.expires_at`; 3008 is handled as a normal drop), temporary
  tokens, `UpdateConfiguration`, speaker labels, `LLMGatewayResponse`.

## Gotchas found during implementation
- serde derives `Deserialize` for structs from JSON arrays too: a `[]` text frame parsed as a typeless object until the
  parser required a leading `{`.
- The contract suite sends 20 ms frames and expects an interim after one frame, so its harness enables the opt-in padded
  flush (`batch_flush = Some(60 ms)`); the adapter's default holds the remainder instead. The mock closes with 3007 like the
  real server when a chunk is outside 50..1000 ms or when audio runs ahead of wall-clock time, so the tests also verify the
  batching and the replay pace.
- Close code and `Error` frame order: the `Error` text frame precedes the close; the adapter logs the frame (without
  content) and lets the close code decide.

Fact-checked 2026-10-08: 68 claims checked, 11 corrected. Corrections: (1) `Turn.words[]` DO have a required `word_is_final`
field (file said no such flag); (2) `language_detection` models per reference/select-model page include 3.5 Pro (file listed only
3.6 Pro + Multilingual; multilingual page disagrees, both now noted); (3) `SpeechStarted` is stated for Universal-3.6 Pro only
(file said "3.x Pro"); (4) the "~1.25x throttle / >5 min buffered -> 3007" claim is not in any official page, now UNVERIFIED and
removed from reconnect guidance; (5) "quickstart recommends chunks of about 50 ms" was wrong (quickstart gives none; 100 ms /
50 ms come from the file-streaming and Deepgram-migration guides); (6) `Error` frame field names `error_code`/`error` could not
be confirmed in the reference (now UNVERIFIED); (7) default model when omitted is also stated by the select-model page
(`universal-3-6-pro`); (8) 3008 "delivered after the current turn / within 60 s" and "extra time billed" not found, now UNVERIFIED;
(9) `universal-3-5-pro` price $0.45/hr added, benchmarks data (3.5 Pro TTCT P50 568 ms / P90 829 ms; none for 3.6 Pro or
Turkish) added, reference page fully read (item 19 resolved), `vad_threshold` 0.5 with `speaker_labels` and `previous_context_n_turns`
default 5 added; (10) `KeepAlive` resets the inactivity timer (clarified), `LLMGatewayResponse` `type` string corrected from
`llmGatewayResponse` message name; (11) unconfirmed wording downgraded to UNVERIFIED: the "process the turn twice" quote, "ADTS",
partial cadence "about every second", "no extra cost" custom limits, volume discounts, "SDK on_error" remark, `keyterms_prompt: []`.
