# Deepgram — STT (live streaming over WebSocket)

Trait: `brisvox_providers::stt::SttProvider` (`SttInput::{Audio, Finalize}`,
`SttEvent::{Transcript{text,is_final,speech_final,from_finalize}, Error}`).

- Docs (all fetched 2026-10-08 unless noted; everything below comes from these pages):
  - Live API reference: https://developers.deepgram.com/reference/speech-to-text/listen-streaming.md
  - Streaming overview: https://developers.deepgram.com/docs/live-streaming-audio and
    https://developers.deepgram.com/docs/stt-streaming-feature-overview
  - Finalize: https://developers.deepgram.com/docs/finalize
  - KeepAlive: https://developers.deepgram.com/docs/audio-keep-alive
  - CloseStream: https://developers.deepgram.com/docs/close-stream
  - Endpointing: https://developers.deepgram.com/docs/endpointing
  - Interim results / is_final vs speech_final: https://developers.deepgram.com/docs/interim-results and
    https://developers.deepgram.com/docs/understand-endpointing-interim-results
  - UtteranceEnd: https://developers.deepgram.com/docs/utterance-end
  - SpeechStarted: https://developers.deepgram.com/docs/speech-started
  - Encoding: https://developers.deepgram.com/docs/encoding.md
  - Keyterm: https://developers.deepgram.com/docs/keyterm
  - Models and languages: https://developers.deepgram.com/docs/models-languages-overview.md
  - Errors / close codes: https://developers.deepgram.com/docs/stt-troubleshooting-websocket-data-and-net-errors.md
    and https://developers.deepgram.com/docs/errors.md
  - Reconnect guide: https://developers.deepgram.com/docs/recovering-from-connection-errors-and-timeouts-when-live-streaming-audio.md
  - Regional endpoints: https://developers.deepgram.com/reference/regional-endpoints.md
  - Rate limits: https://developers.deepgram.com/reference/api-rate-limits
  - Auth: https://developers.deepgram.com/reference/authentication.md and
    https://developers.deepgram.com/guides/fundamentals/token-based-authentication.md
  - Latency: https://developers.deepgram.com/docs/measuring-streaming-latency.md
  - Flux: https://developers.deepgram.com/docs/flux/quickstart.md and
    https://developers.deepgram.com/docs/flux/language-prompting.md
  - Pricing: https://deepgram.com/pricing (fetched 2026-10-08)

## Auth
- `Authorization: Token <DEEPGRAM_API_KEY>` (the authentication page documents exactly this form,
  `Authorization: Token <YOUR_DEEPGRAM_API_KEY>`; the Flux quickstart shows the same form for the WebSocket). The
  live streaming reference spec only says "an `Authorization` header" without the prefix.
- Short-lived JWT alternative: `Authorization: Bearer <JWT>`; `/listen` WebSocket accepts temporary tokens, valid
  only during the initial connection; default TTL 30 s, max `ttl_seconds` 3600 (token-based-authentication page).
- Browser `sec-websocket-protocol` subprotocol auth: UNVERIFIED (not covered by any fetched page; brisvox is a
  server-side client so the header is enough).
- Failed upgrade: HTTP 4xx/5xx instead of 101; response headers `dg-request-id` (always present) and `dg-error`
  (present on failed upgrades, contains the error message) (STT troubleshooting page); the same message in the HTTP
  body is from Deepgram's debugging blog/changelog, not the troubleshooting page.

## Endpoint(s) / regions
- Global: `wss://api.deepgram.com/v1/listen` (Nova/Base/Enhanced family).
- Regional hostnames (same API keys, same paths, strict routing, "Requests to the EU endpoint are never routed
  outside the EU"; if a region is down, requests fail rather than fall back): EU `api.eu.deepgram.com`, AU
  `api.au.deepgram.com`, India `api.in.deepgram.com`. Pattern `wss://api.{region}.deepgram.com/v1/listen`. All three
  regions support `/v1/listen` and `/v2/listen` (excluding Whisper). Plan constraints for regional endpoints: the
  page lists none (it only suggests Deepgram Dedicated for hosting in a specific EU country).
- Flux (turn-taking model) uses a different endpoint: `wss://api.deepgram.com/v2/listen` (`/v1/listen` will not
  work with Flux). See Flux section.

## Query parameters (v1/listen; table from the live reference)
Defaults as stated by the reference. Brisvox-relevant ones:
| Param | Values / default |
|---|---|
| `model` | `nova-3` (also `nova-3-general`, `nova-3-medical`), `nova-2*`, `nova`, `enhanced`, `base`, ... ; no default listed in reference or models page; the live-streaming-audio page says omitting it uses "the default model, which is currently `model=base`" |
| `language` | default `en`; Turkish = `tr` (Nova-3 also lists `tr-TR`) |
| `encoding` | `linear16`, `linear32`, `flac`, `alaw`, `mulaw`, `amr-nb`, `amr-wb`, `opus`, `ogg-opus`, `speex`, `g729`; for raw headerless audio `encoding` AND `sample_rate` are both required |
| `sample_rate` | integer Hz, required together with `encoding` for raw audio |
| `channels` | default `1` |
| `interim_results` | `true`/`false`, default `false` |
| `endpointing` | ms integer, or `false` to disable; reference default `10` |
| `utterance_end_ms` | 1000..5000 ms (min 1000 on cloud); requires `interim_results=true` |
| `vad_events` | `true`/`false`, default `false` (enables `SpeechStarted`) |
| `smart_format` | `true`/`false`, default `false` |
| `punctuate` | `true`/`false`, default `false` |
| `keyterm` | Nova-3 keyterm prompting (limit 500 tokens per request, also applies to each mid-stream update; keyterm page says it also works with Flux); `keywords` is legacy (other models such as Nova-2) |
| `numerals`, `diarize_model`, `redact`, `replace`, `search`, `tag`, `extra`, `callback`, `multichannel`, `profanity_filter`, `detect_entities`, `dictation` | not needed for brisvox v1 (names not individually re-verified) |
| `version` | default `latest` |
| `mip_opt_out` | model improvement program opt-out, default `false` |
Recommended brisvox set (derived from docs, not copied): `model=nova-3&language=tr&encoding=linear16&sample_rate=16000
&channels=1&interim_results=true&endpointing=300&vad_events=true&smart_format=true` (+ `utterance_end_ms=1000`
only as a safety net, see caveats). For telephony pass-through: `encoding=mulaw&sample_rate=8000`
(the encoding page gives no mulaw/alaw rate guidance; 8000 for G.711 telephony is UNVERIFIED in Deepgram docs).
Audio chunk size: docs say streaming buffers "between 20 and 100 milliseconds" (latency page); Flux page strongly
recommends 80 ms.

## Client -> server messages
Audio: binary WebSocket frames (raw bytes for the chosen `encoding`). Control messages are TEXT frames (JSON):
```json
{"type": "KeepAlive"}
{"type": "Finalize"}
{"type": "Finalize", "channel": 0}
{"type": "CloseStream"}
```
- `Finalize` with optional `channel` (0-based; omitted = all channels). KeepAlive must be a text frame; sending it
  as binary "may result in incorrect handling and potential connection issues".
- The reference page gives only field names/types, no literal JSON examples (the shapes above and below are derived
  from its schema, except the `Finalize` channel example which is on the finalize page).
- `Configure` (Nova-3 only) — shape from the reference schema (not a verbatim docs example):
  `{"type":"Configure","keyterms":["term one","term two"],"features":{"numerals":true}}`. `keyterms` replaces the
  whole list (including keyterms set via the `keyterm` query param); empty array clears; omitted/null keeps the
  current list. Other models return an `Error` with code `KeytermsNotSupported`. On the EU/AU/India regional
  endpoints, mid-stream `keyterms` in `Configure` is not available yet (returns an `Error`, stream keeps its
  keyterms; regional-endpoints page) — this is region-specific, not a contradiction; set keyterms via the `keyterm`
  query parameter there (and simplest everywhere).

## Server -> client messages (all JSON text frames, discriminated by `type`)
- `Results`: required fields `type`, `channel_index` (`[idx, total]`), `duration`, `start`, `channel`, `metadata`;
  optional `is_final`, `speech_final`, `from_finalize`, `entities`. `channel.alternatives[]` (`transcript`,
  `confidence`, `languages[]`, `words[]` with `word`, `start`, `end`, `confidence`, `language`, `punctuated_word`,
  `speaker` only with diarization), `metadata` (`request_id`, `model_uuid`, `model_info{name,version,arch}`,
  `diarize_info` only when a diarizer ran), `entities[]` (`label`, `value`, `raw_value`, `confidence`,
  `start_word`, `end_word`) per the reference. Docs example (interim; example omits `type`):
```json
{"channel_index":[0,1],"duration":1.039875,"start":0,"is_final":false,
 "channel":{"alternatives":[{"transcript":"another big","confidence":0.9600255,
 "words":[{"word":"another","start":0.2971154,"end":0.7971154,"confidence":0.9588303},
          {"word":"big","start":0.85173076,"end":1.039875,"confidence":0.9600255}]}]}}
```
  Endpoint example (endpointing page, trimmed): `{"duration":1.039875,"start":0.0,"is_final":false,"speech_final":true, ...}`
  (note `is_final` is false there; see semantics).
- `UtteranceEnd` (needs `interim_results=true` + `utterance_end_ms`):
```json
{"channel": [0, 1], "last_word_end": 2.395, "type": "UtteranceEnd"}
```
  `last_word_end == -1` means the result was already finalized before the `utterance_end_ms` condition was met:
  ignore it.
- `SpeechStarted` (needs `vad_events=true`):
```json
{"type": "SpeechStarted", "channel": [0, 1], "timestamp": 9.54}
```
  Its `timestamp` may differ from the first word's start (separate systems); complements, does not replace, VAD.
- `Metadata`: `type`, `transaction_key` (deprecated), `request_id`, `sha256`, `created`, `duration`, `channels`.
  Sent as the summary after `CloseStream`.
- `Error` (non-fatal; fatal failures close the socket): `type`, `variant` (required; `SchemaError` = unparsable
  client text message, `message` holds the original; `InvalidConfigureMessage` = rejected Configure), `description`
  (required), optional `code` (e.g. `KeytermsNotSupported`), optional `message`.

## Result semantics (mapping to brisvox `SttEvent::Transcript`)
- `is_final=false`: interim, replaces the in-progress text (not append). Words near the end may be truncated
  (`word.end == duration`).
- `is_final=true`: this audio segment is fixed; it does not by itself mean the speaker has finished. One utterance
  can contain several `is_final=true` results.
- `speech_final=true`: endpointing detected a pause (utterance end). Docs: "Do not use `speech_final: true` alone
  to capture full transcripts." Algorithm per docs: append every `is_final:true` transcript to a buffer; when
  `speech_final:true` arrives the buffer is the full utterance; clear it.
- Docs' own endpointing example shows `is_final:false` together with `speech_final:true`; the page does not
  explain it. Treat `speech_final` with `is_final=false` as UNVERIFIED semantics (implementation should not emit
  a final text from such a message; safest: only treat is_final=true as text, and let a following/previous
  `is_final` flush the buffer).
- `from_finalize=true` marks results produced in response to `Finalize`.

## Keepalive / timeouts
- Send `{"type":"KeepAlive"}` every 3-5 s when no audio is being sent; no server response.
- Closes with `NET-0001` if no audio or KeepAlive for 10 s (KeepAlive page). But the STT troubleshooting page
  (not the errors page) also says for NET-0001: send audio within 10 seconds of opening the connection;
  "KeepAlive messages alone won't prevent this closure, so you must send at least one audio message". For
  `NET-0002` (no audio within the no-audio timeout window, close reason `no_audio_timeout`): send audio or
  KeepAlive; KeepAlive messages DO reset this timeout. The reconnect guide also says to stream audio within 10 s of
  opening.
  => Always send audio (even silence) soon after connect; KeepAlive is for pauses after audio has flowed.
  Whether a KeepAlive-only pause is indefinitely allowed after audio has flowed: the streaming overview says
  KeepAlive can "pause" the connection without closing it; the exact max pause (and the no-audio timeout
  duration for NET-0002) is UNVERIFIED.
- Word timings are relative to the audio stream, not the connection lifetime.

## Finalize / flush / cancel semantics
- `Finalize`: "forces the server to immediately process any unprocessed audio data and return the final
  transcription results", ensuring interim results are treated as final; results may carry `"from_finalize": true`.
  The docs say this response "is not guaranteed if there is no significant amount of
  audio data to process" -> the client must not wait forever for a `from_finalize` result (use a timeout; the
  brisvox adapter should synthesise the end of utterance itself, e.g. after the last final received or a short
  deadline). Whether `speech_final` is set on the from_finalize result: UNVERIFIED (the finalize page does not
  mention `speech_final`; the brisvox trait says the last from_finalize transcript is `speech_final`, so the
  adapter must set that itself).
- `CloseStream`: server processes all remaining audio, returns final results, sends a `Metadata` summary, then
  terminates the WebSocket. Docs say to always send it when done. Behaviour if sent before any audio: the page's
  example Metadata shows `duration` 0.0 / `channels` 0 for nothing-left-to-process; other behaviour UNVERIFIED.
- There is no "cancel/discard buffered audio" message; to discard, close the socket and reconnect.

## Timestamps / interim results
- `start`/`duration` on Results and per-word `start`/`end` are seconds from the start of the audio sent on this
  connection. They are not guaranteed accurate at millisecond level (latency page). Timestamps restart at 0 on
  every new connection.
- Interim results are "typically sent every second" (UtteranceEnd page) so `utterance_end_ms` < 1000 is pointless.

## Endpointing config
- `endpointing=<ms>` waits that much silence before finalizing and setting `speech_final`; `endpointing=false`
  disables it (transcripts then arrive at a cadence determined by Deepgram's chunking algorithms). Docs: enabled by
  default at 10 ms (endpointing page; the understand-endpointing page says this suits chatbots expecting short
  utterances); 300-500 ms suits conversations where speakers pause mid-thought (understand-endpointing page; the
  endpointing page's own code samples use 300). Whether 10 ms is the true live default: verify against live
  behaviour (UNVERIFIED beyond the docs).
- `UtteranceEnd` fires when a gap of `utterance_end_ms` follows the last finalized word. Claim that it can fire
  while the speaker continues and that client-side logic may be better for voice agents: UNVERIFIED (not seen in
  the re-fetched UtteranceEnd page). Brisvox uses its own VAD (Silero + Smart Turn) and `Finalize`; provider
  endpointing is a fallback.

## Concurrency / rate limits (streaming STT, per project; keys in one project share the pool)
- Pay As You Go: up to 150 concurrent streams (Nova and Flux). Growth: North America 225, EU/AU/IN 150.
  Enterprise: Flux up to 300, Nova starting at 300. Secondary self-serve projects: 1 concurrent stream.
  (Streaming diarization has separate lower limits: 50 NA / 25 EU-AU-IN on PAYG and Growth.)
- Over-limit response: HTTP 429 `TOO_MANY_REQUESTS`; the errors page recommends an exponential-backoff retry
  strategy for the Speech-to-Text 429; the rate-limit page itself does not state the behaviour and documents no
  rate-limit headers.
- Audio ingest cap: Deepgram accepts audio at most 1.25x realtime (reconnect guide) — replay backlog is bounded.

## Errors and close codes
- WebSocket close: `1008` + `DATA-0000` (payload cannot be decoded as audio; check `encoding`/`sample_rate`);
  `1011` + `NET-0000` (service has not transmitted a Text frame to the client within the timeout window; may be an
  internal Deepgram issue or insufficient audio to transcribe; docs: "Retry your request"); `1011` + `NET-0001`
  (service has not received a Binary or Text frame from the client within the timeout window); `1011` + `NET-0002`
  (no audio within the no-audio timeout window, close reason `no_audio_timeout`).
- HTTP errors at handshake (errors page): 400 (`INVALID_QUERY_PARAMETER`, `Bad Request`, ...), 401 `INVALID_AUTH`,
  402 `ASR_PAYMENT_REQUIRED`, 403 `INSUFFICIENT_PERMISSIONS` (project lacks model access), 429 `TOO_MANY_REQUESTS`,
  503 `Service Unavailable`. JSON body fields: `err_code`, `err_msg`, `request_id` (the TTS 503 example uses
  `error_code`: parse both). The 400 Invalid JSON example uses `category`, `message`, `details`, `request_id`.
- Normal close code after `CloseStream` (1000?): UNVERIFIED (close-stream page specifies none).

## Reconnect (per the recovery guide)
- "your application must initialize a new WebSocket connection and start a new streaming session"; send audio
  within 10 s or it closes.
- Audio produced during an outage is lost unless the client buffers it; replay the buffer after reconnect. Large
  backlogs add delay because Deepgram accepts audio at most 1.25x realtime.
- Timestamps restart at 00:00:00 on the new connection: keep a start offset and add it to returned timestamps.
- The guide lists no other state; implicitly lost: interim hypotheses, un-finalized audio in flight, the
  utterance buffer, `Configure`d keyterms/feature changes (re-apply via query params). No retry/backoff values or
  close-code handling are specified by the guide: UNVERIFIED (use bounded exponential backoff for 429/503/1011).
- Brisvox consequence: keep a bounded ring of audio sent since the last `speech_final`/finalize; on reconnect
  replay it (paced <= 1.25x realtime), discard in-flight interims, and add the offset to word timestamps.

## Models and Turkish
- Turkish: Nova-3 `tr`, `tr-TR`; Nova-2 `tr`; Base `tr`. Enhanced and Nova (1): not listed. Flux: not listed.
- Nova-3 `language=multi` covers only English, Spanish, French, German, Hindi, Russian, Portuguese, Japanese,
  Italian, Dutch — NOT Turkish. For Turkish pass `language=tr` (monolingual).
- Current Nova-3 name in docs: `nova-3`; the models page does not mention any "Nova-4" and calls Flux the
  "latest-generation streaming model".
- Model version pinning via `version` (default `latest`).

## Flux (turn-taking STT) — does NOT change the Turkish setup
- Models `flux-general-en` (English) and `flux-general-multi` (10 languages: en, es, fr, de, hi, ru, pt, ja,
  it, nl). Turkish is NOT supported by Flux (`language_hint=tr` would give 400 `INVALID_PARAMETER`). Plain
  `model=flux`: not documented (UNVERIFIED; use the two ids above).
- Endpoint `wss://api.deepgram.com/v2/listen`. Params: `eot_threshold` (0.5-1.0, default 0.7),
  `eager_eot_threshold` (0.3-0.9, off by default; required to enable `EagerEndOfTurn` and `TurnResumed`),
  `eot_timeout_ms` (500-60000, default 5000), `language_hint`
  (repeatable, only `flux-general-multi`; other models return 400), `encoding`/`sample_rate` for raw audio (linear16,
  linear32, mulaw, alaw, opus, ogg-opus; rates 8000/16000/24000/44100/48000; 16000 recommended; 80 ms chunks, 2560
  bytes at 16 kHz linear16).
- Messages: `Connected`, `TurnInfo` (transcript, per-word `word`/`confidence`/times, languages; events
  `EndOfTurn`, `EagerEndOfTurn`, `TurnResumed`; the quickstart does not mention an `Update` event, but the latency
  page says to count Flux interim results via `"type": "Update"` — the two pages disagree/are unclear: UNVERIFIED);
  control: `Configure` (updates `eot_threshold`, `eager_eot_threshold`, `eot_timeout_ms` mid-stream; per
  language-prompting page also `language_hints` plural, `[]` clears), `ForceEndTurn` (payload undocumented). Exact
  JSON/field names (e.g. `event`, `turn_index`): UNVERIFIED.
- `EagerEndOfTurn` can raise LLM calls by 50-70% (speculative responses).
- Decision input: brisvox v1 targets Turkish, so Nova-3 `/v1/listen` + own VAD/Smart Turn + `Finalize`. A Flux
  adapter could be a later option for English/multi languages only.

## Pricing
- https://deepgram.com/pricing (2026-10-08, streaming per minute; labelled "limited-time promotional rates"):
  Nova-3 monolingual PAYG $0.0048 (regular $0.0077), Growth $0.0042 (regular $0.0065); Nova-3 multilingual PAYG
  $0.0058 (regular $0.0092) / Growth $0.0050 (regular $0.0078); Flux English PAYG $0.0065 (regular $0.0077) /
  Growth $0.0057 (regular $0.0065); Flux Multilingual PAYG $0.0078 / Growth $0.0068 (single price listed). Recheck
  before quoting.

## Gotchas / implementation notes
- Reference lists `Authorization` header only; use the `Token` prefix for API keys.
- Control messages must be text frames; audio binary frames. Never log the key or the `Authorization` header.
- Connection closes at 10 s without audio even with KeepAlive if audio never started — send audio immediately
  (e.g. 20 ms of silence) after connect; `warm()` should therefore not hold an idle socket open.
- `speech_final` can arrive on a result whose `is_final` is false per the endpointing example (unexplained).
- UTF-8 Turkish text arrives in `transcript`; `punctuated_word` holds the punctuated form when formatting is on.
- Deepgram does not store transcripts; the response is the only chance to get them (live-streaming-audio page).

## UNVERIFIED summary
1. `sec-websocket-protocol` browser auth. 2. Whether `speech_final` is set on `from_finalize` results and what
`Finalize` does with little audio (docs: response not guaranteed). 3. Exact semantics of `is_final:false` +
`speech_final:true`. 4. Max KeepAlive-only pause duration after audio started, and NET-0002 timeout duration.
5. Normal close code after `CloseStream`. 6. mulaw/alaw `sample_rate` guidance (8000 assumed). 7. Flux exact JSON
field names and whether an `Update` event exists. 8. Reconnect retry/backoff values and state list. 9. Reference
lists endpointing default 10 ms; verify live. 10. `UtteranceEnd` "can fire while speaker continues / client-side
logic better" wording. 11. Plain `model=flux` validity. 12. 8000 Hz for telephony. 13. Individual names of the
"not needed" query parameters.

Fact-checked 2026-10-08: 62 claims checked, 15 corrected. Corrections: (1) auth Token prefix is documented on the
authentication page (was "UNVERIFIED-ish"); (2) default model `base` attributed to live-streaming-audio page, not
the overview/models page; (3) Configure keyterms "contradiction" resolved as region-specific (EU/AU/IN) behaviour;
(4) KeepAlive-alone/NET-0001 statements re-attributed from errors page to STT troubleshooting page, and NET-0002
KeepAlive-resets added; (5) NET-0000 meaning corrected (service sent no Text frame to client; not just "internal
issue"); (6) removed unsupported claim that the endpointing page flags 10 ms as surprising (10 ms default is on both
pages; 300-500 ms advice is on the understand-endpointing page); (7) removed unsupported UtteranceEnd "can fire
while speaker continues" claim, marked UNVERIFIED; (8) errors page does not contain the phrase "Retry with
exponential backoff" (it recommends an exponential-backoff strategy for STT 429); (9) the "Retry your request" and
dg-request-id/dg-error facts re-sourced to the troubleshooting page (HTTP-body copy of the error is from the
Deepgram blog); (10) timestamps are "not guaranteed accurate at millisecond-level" (was "not millisecond
accurate"); (11) Enterprise concurrency clarified (Flux up to 300, Nova starting at 300); (12) Flux `Update` event
not in quickstart (latency page conflict noted); (13) pricing: added missing regular rates for Nova-3
multilingual and Flux English; (14) keyterm 500-token limit also applies to mid-stream updates and works on Flux,
`Configure` also supports `language_hints` on Flux; (15) Results required/optional fields, Error required fields,
and example-omits-`type` note added per reference.

## Adapter: `brisvox_providers::deepgram` (feature `provider-deepgram`)
Code: `crates/brisvox-providers/src/deepgram/` (`config`, `protocol`, `assembler`, `replay`,
`connection`, `session`). Tests: `crates/brisvox-providers/tests/deepgram.rs` (local mock of the wire
protocol on a loopback `TcpListener`, the shared contract suite, one live test).

**API.** `DeepgramStt::new(DeepgramConfig, SharedClock)` implements `SttProvider`.
`DeepgramConfig::new(Secret<String>)` holds the defaults; fields: `model` (`nova-3`), `language`
(overridden by `SttConfig.language`), `encoding` (`Linear16` | `Mulaw`) and `sample_rate_hz`
(default: `SttConfig.sample_rate`), `interim_results` (true), `endpointing` (`Ms(300)` | `Off`),
`utterance_end_ms`, `smart_format` (true), `punctuate`, `keyterms`, `region` (`Global` | `Eu` | `Au` |
`India`), `base_url` (tests; strictly `host[:port]` after the scheme, so no userinfo `@`, path, query or
backslash; `ws://` only for the loopback hosts `localhost`, `127.0.0.1`, `::1`), and the time settings `io_timeout` (10 s),
`keepalive_interval` (4 s, must be below 10 s), `finalize_timeout` (800 ms), `finalize_settle_gap`
(500 ms, `ZERO` disables), `close_timeout` (1 s), `max_reconnects` (1),
`reconnect_backoff` (100 ms, doubling, cap 5 s), `replay_window` (8 s), `replay_speed` (1.25).

**Query.** `model, language?, encoding, sample_rate, interim_results, endpointing,
utterance_end_ms?, smart_format, punctuate?, keyterm*` in that fixed order, values percent-encoded;
`model`/`language` of the config are restricted to `[A-Za-z0-9._-]`.

**Mapping.** Interims: forwarded as interim transcripts with the text of the current segment; blank
interims are dropped; if the events channel is full an interim is dropped (finals and errors wait,
bounded by `io_timeout`). Finals: every `is_final` result with text is one final segment (the consumer
appends); `speech_final` is forwarded. `from_finalize` results always carry `speech_final=true`
(the doc does not say Deepgram sets it). `UtteranceEnd` (`last_word_end != -1`) closes an open
utterance (the last interim is promoted to a final if no final is pending). Blank finals with
`speech_final` only matter as an utterance end somebody waits for (open finals or a Finalize) and
are otherwise dropped. `is_final=false` + `speech_final=true` (unexplained in the docs): if finals of
the utterance were already forwarded it closes the utterance with an empty `speech_final` final,
otherwise the flag is given to the next final; it never turns interim text into final text.
`SpeechStarted`, `Metadata` and unknown message types are ignored; a missing `type` is read as `Results`.

**Finalize.** `{"type":"Finalize"}` goes out at the position of the request in the audio stream. A
second request while one is open is ignored. Every Finalize gets exactly one `from_finalize`
transcript with `speech_final`, from one of three sources:
1. Deepgram's `from_finalize` result (the discriminator; an ordinary endpointed final that arrives
   while the Finalize is open is forwarded as what it is, `from_finalize=false`, and does not
   answer it). A final whose end already reaches the Finalize position answers it too, since the
   server then has nothing left to flush. The tolerance depends on whether the Finalize has gone
   out: once it is on the wire the server holds it anyway, so a final ending within
   `finalize_settle_gap` of the position answers it; while it is still queued (behind a replay) only
   a final within 20 ms of the position does (timestamp rounding), otherwise the Finalize is still
   sent and the final is an ordinary segment. (A wide tolerance on a queued Finalize would drop it
   and strand the speech between the final's end and the Finalize position.)
2. The adapter, after `finalize_timeout` counted from the *request* (not from when the message goes
   out), so a reconnect plus replay in between does not stretch the wait:
   `Transcript{text: last interim or "", is_final, speech_final, from_finalize}`. A Finalize that
   was still queued behind a replay stays queued and is sent later; its answer is then a late one.
3. The adapter, at once and without sending anything, when the Finalize finds the recognizer
   idle: no open finals, no interim text, and the latest `Results` message of the connection ends
   within `finalize_settle_gap` of the end of the audio sent (the usual case when endpointing fired
   before the engine's VAD or Smart Turn stop; it avoids the full 800 ms). It requires at least one
   `Results` message on the connection, so a call that has heard nothing yet still waits for
   Deepgram. The gap is a guess (see below): speech newer than it is reported as a late utterance.

A *late* `from_finalize` result (after source 2 or 3) is blank: dropped; non-blank with the same
words as the synthetic final (case, punctuation and spacing ignored; Turkish casing included:
`I`/`ı` and `İ`/`i` compare equal, so a smart-formatted `Işık geldi.` matches the interim `ışık geldi`): dropped as a duplicate (also a
real final with the same words when Deepgram ignored the Finalize and endpointing fired later; the
record ends with any new interim, the next Finalize, or its first match); otherwise real text the
synthetic final lacked, delivered as another utterance end.

**Keepalive and silence.** No audio for `keepalive_interval` -> `{"type":"KeepAlive"}` (repeated).
A connection that has seen no audio at all gets 20 ms of digital silence once instead (zeros for
linear16, `0xFF` for mu-law), because KeepAlive alone does not prevent the `NET-0001` close.

**Watchdogs (all `Clock`-based).** Connect (TCP+TLS+upgrade together) and every write: `io_timeout`
(connect -> `Core(Timeout)`, a failed or timed-out write -> connection lost). After audio was sent the
server must send some message within `io_timeout` of the last one (or of the audio that started the
wait): otherwise the connection is treated as lost, like a closed socket (a half-open socket or a
server that stopped answering without a close frame is the commonest real drop, and `NET-0000` for
"no Text frame" is documented as retriable), and replaced with a replay. Only when no reconnect is
left does the session end, with `Core(Timeout{what: "deepgram response"})` instead of `Disconnected`.
The watchdog is off once a KeepAlive had to be sent (a pause in the audio) until audio resumes.
`keepalive_interval` must be below 10 s (`NET-0001`; rejected otherwise) and should be below
`io_timeout`: otherwise a pause in the audio longer than `io_timeout` looks like a silent server (a
warning is logged; it is not rejected because the shared contract suite needs a keepalive slower than
its 600 ms deadline). A caller who is silent for longer than `io_timeout` while the server also
sends nothing costs one reconnect (with a replay of at most `replay_window`); raise `io_timeout` if
Deepgram turns out not to send blank interims during silence.

**Errors.** Upgrade answered with HTTP status N -> `Rejected{N}` (headers `dg-request-id`/`dg-error` are
logged, never the request); close `1008` -> `Rejected{1008}` and not retried (undecodable audio);
an unparsable message or invalid frame -> `Protocol` (no message content in the error); an `Error`
message: documented as non-fatal, so only the two variants that mean the adapter sent something
wrong (`SchemaError`, `InvalidConfigureMessage`) end the session, as `Protocol` with the variant and
the (truncated, 200 chars) description in `detail` (never the echoed client message); any other
variant is logged (`warn`, variant only) and the session continues;
other closes (`1011` NET-0000/1/2, `1000`), socket errors, EOF -> connection lost.

**Reconnect.** A lost connection is replaced up to `max_reconnects` times (the counter restarts
when a `Results` message arrives, the only message that proves a working connection: a server that
accepts, sends `Metadata` and closes, over and over, is not followed forever; backoff
`reconnect_backoff * 2^attempt`, capped at 5 s; the attempt times are asserted in the tests). Credentials errors
(401-403) are surfaced as `Rejected` at once; otherwise, when the attempts run out, `Disconnected`.
The adapter keeps a ring of the audio sent since the server last finalized anything (bounded by
`replay_window`, oldest chunk first; refcounted `Bytes`, no copy). Final results trim the ring to the
end of the finalized audio: `start + duration` seconds are bytes on this connection, shifted by the
connection's base position in the session's audio stream (timestamps restart at 0 per connection) and
minus the generated silence. A completed Finalize (answered by `from_finalize`, or given up on after
`finalize_timeout`) trims to the position where it was sent; tests drop the connection after each and
check that only newer audio is replayed. After a
reconnect the ring is replayed paced at `replay_speed` (the docs: at most 1.25x; the ring's front is
always on a sample boundary, also for odd-sized caller chunks), a pending Finalize is
re-sent at its original position (its `finalize_timeout` deadline is not restarted), interims are forgotten, and audio/Finalize from the caller keep being
accepted meanwhile. Backlog `B` s drains in `B / (replay_speed - 1)` s while live audio keeps arriving.

**Hang-up.** Dropping the session input (or the events receiver) closes `events` at once, then sends
`CloseStream` and waits at most `close_timeout` for the server. Results the server flushes in answer to
`CloseStream` are discarded (the contract: nothing follows a hang-up).

**`warm()`.** Builds the process-wide rustls config (`net::tls_config()`) and resolves the host
within `io_timeout`. It does not pre-connect: an idle connection is closed after 10 s without audio.
All sessions share the TLS config, so rustls' in-memory session store can resume TLS for later connects.

### Adapter decisions that rest on UNVERIFIED points
- `speech_final` on `from_finalize` results: not relied on (set by the adapter).
- `finalize_settle_gap` (500 ms): chosen as roughly `endpointing` (300 ms) plus recognition latency;
  how far an endpointed final's `start + duration` lags the end of the speech, and how often blank
  interims arrive during silence, are not documented. Both only decide how often the shortcut applies.
- `is_final=false` + `speech_final=true`: both readings handled (see Mapping).
- Whether the server sends any text frame while it receives silent audio: not documented. The
  watchdog assumes it does (the `NET-0000` close, "no Text frame to the client within the timeout
  window", suggests so). If it does not (e.g. `interim_results=false` and a very quiet caller), a
  quiet stretch longer than `io_timeout` costs a reconnect with replay (and a second one in a row
  ends the session with `Core(Timeout)`); raise `io_timeout` then.
- The KeepAlive-only pause limit and the `NET-0002` window: the adapter just sends KeepAlive.
- Normal close code after `CloseStream`: not relied on (any close or EOF ends the wait).
- 8000 Hz for mu-law: taken from `SttConfig.sample_rate` unless `sample_rate_hz` overrides it.

## Gotchas found during implementation
- `tokio_tungstenite::Connector` is a different type from `tungstenite::Connector`;
  `connect_async_tls_with_config` wants the tokio one.
- The tungstenite defaults buffer writes (128 KiB) and the read buffer is 128 KiB: the adapter sets
  `write_buffer_size(0)` (each `send` flushes), `read_buffer_size(16 KiB)`, `max_message_size(1 MiB)` and
  `TCP_NODELAY`.
- The handshake future is large; polled inline it needed more than 4 MiB of stack in a debug build when
  nested in the contract suite's futures. It is `Box::pin`ned in `connection::open`.
- Timestamps are per connection, and the 20 ms of generated silence is part of that audio: the offset math
  must subtract it, or the trim after a reconnect lands too late and drops unfinalized audio.
- A `Results` example in the docs has no `type`; do not require it.
- `Finalize` without enough buffered audio is simply not answered (docs), and a late answer can still
  arrive after the synthetic final: a late blank one is dropped, a late one with the same words is a
  duplicate and dropped, a late one with other words is delivered as another utterance end.
- Only `from_finalize` (or a final that reaches the Finalize position) answers a Finalize. Treating
  every `speech_final` as the answer mislabels a normal endpointed final and turns the real answer
  for the remaining audio into a phantom second utterance.
- A server that closes with `1011` right after connect (`NET-0001`: no audio) must not make the adapter
  loop: the reconnect attempt counter is only reset by a `Results` message (a `Metadata` or ignored
  message does not count).
- The contract suite's stall checks expect `Core(Timeout)` at the deadline, which a reconnect-on-silence
  adapter only gives when no reconnect is left: the harness in `tests/deepgram.rs` runs with
  `max_reconnects = 0` (the reconnecting behaviour has its own tests).
- Eviction from the replay ring cuts whole caller chunks; with odd-sized 16-bit chunks that would leave
  the replay start inside a sample (every following sample byte-swapped, then `1008`), so the front is
  rounded up to the sample size.
- A socket that disappears without a closing handshake surfaces as a tungstenite protocol/IO error or EOF;
  the adapter treats all of them as "connection lost".
- The live test (`BRISVOX_LIVE_TESTS=1`, `DEEPGRAM_API_KEY`) settles UNVERIFIED items 2 and 3 (does
  Deepgram answer `Finalize`, and with `speech_final`?). It sets `finalize_timeout` to 8 s and
  `finalize_settle_gap` to zero, so the adapter's own fallbacks cannot answer, and requires the
  `from_finalize` + `speech_final` event within 3 s of the request; the measured delay is printed.
  Without the env vars it prints "skipped" and passes: it proves nothing until run live.
- The shared contract suite takes about 6.6 s of real time because its task-leak checks wait for fixed
  2 s quiet windows; the timeout checks run with a 600 ms adapter deadline instead of 2 s.

## Live check, Turkish and interims (2026-10-13)
Source re-fetched 2026-10-13: https://developers.deepgram.com/docs/models-languages-overview lists Turkish for
`nova-3` / `nova-3-general` (`tr`, `tr-TR`, monolingual), `nova-2` (`tr`) and `base` (`tr`); not for Flux, Enhanced or the
Nova-3 medical variants. The adapter default `nova-3` + `language=tr` is therefore the right Turkish setting.

Measured (`crates/brisvox-providers/tests/live_checks.rs`, nova-3, `language=tr`, linear16 16 kHz):
- FLEURS `tr_two_utterances.wav`: WER 0.038 and 0.000 (two runs), `en_two_utterances.wav` (`language=en`): WER 0.103.
  Turkish TTS audio (Cartesia, ElevenLabs, resampled 24 -> 16 kHz): WER 0.00-0.31, mostly number and spacing effects
  (`bench/results/live-checks-2026-10-13.md`).
- Interim cadence: one interim about every second (0.9-1.1 s), an `is_final` segment every 3-6 s on read speech that
  restarts the interim text (the interim holds only the text since the last final). Confirms "typically every second".
- `Finalize` sent at a VAD stop is answered with `is_final` + `speech_final` + `from_finalize` (as the adapter
  delivers it) 290 ms (EN) / 330 ms (TR) later (median of 11 turns, max 373 ms). Whether the adapter's synthetic fallback or Deepgram produced each of them was not
  logged. Without `Finalize`, `speech_final` came ~600 ms after the stop (endpointing 300 ms plus the recognizer).
  An answer to `Finalize` on real speech arrives within the ~330 ms seen here (UNVERIFIED items 2 and 3 were checked only with a tone before).
- The last interim before the final equals the final in only 1 of 17 utterances (EN and TR alike): see
  `bench/results/speculation-live-deepgram-2026-10-13.md`; `on_partial` speculation is not recommended.
- Still UNVERIFIED: browser subprotocol auth, close codes, KeepAlive-only pause length.
