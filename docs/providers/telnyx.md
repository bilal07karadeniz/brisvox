# Telnyx Media Streaming (bidirectional) — telephony transport

Scope: the Telnyx counterpart of the Twilio adapter (verify docs). Twin of `docs/providers/twilio.md` (read that first
for the shared design: mu-law/A-law via `docs/providers/g711.md`, buffer-clear on interruption,
playback tracking via marks feeds played-text truncation). Related: `docs/providers/call-control.md` (DTMF, transfer).
Researched: 2026-10-08. Every fact below was read from the pages listed under Sources on that date.
Anything not confirmed is marked UNVERIFIED. This is a transport, not an STT/LLM/TTS provider:
Telnyx is the WebSocket CLIENT, brisvox is the WebSocket SERVER (same role as Twilio).

How the facts were obtained: the Telnyx pages were read through a summarizing fetch tool. The
authoritative JSON examples and schemas below come from the AsyncAPI 3.0.0 spec (v2.0.0) that
Telnyx links from its docs (https://telnyx-openapi-ng.s3.us-east-1.amazonaws.com/media-streaming/media-streaming.yml,
read raw on 2026-10-08), so field names, types, enums and examples are exact. Prose rules (limits,
mark semantics) are quoted/paraphrased from the guide and API reference. The guide's own JSON
blocks could not be reproduced by the tool; they were checked only as key lists and agree with the spec.

- Docs: https://developers.telnyx.com/docs/voice/programmable-voice/media-streaming (fetched 2026-10-08);
  WebSocket spec https://developers.telnyx.com/api-reference/websockets/stream-call-media-over-websocket
  (fetched 2026-10-08); full source list at the bottom.
- Auth / trust:
  - Starting a stream (REST): `Authorization: Bearer <API key>` on `POST https://api.telnyx.com/v2/...`.
    That is Telnyx's API, not brisvox's; brisvox core does not hold it unless it also hangs up or
    transfers via REST (see "Ending the call").
  - The WebSocket itself has NO signature (no `X-Telnyx-Signature`-style header is documented for the
    upgrade). The only documented authenticity mechanism is an optional shared token: the
    `stream_auth_token` field of `streaming_start` and of `dial` (string, max length 4000; streaming_start reference:
    "An authentication token to be sent as part of the WebSocket connection"). When configured, Telnyx sends it as the
    WebSocket upgrade header `x-telnyx-streaming-auth-token` AND in the first frame as
    `connected.x-telnyx-streaming-auth-token` (spec: "When a streaming auth token is configured, Telnyx includes it in the
    `connected.x-telnyx-streaming-auth-token` field and sends it as the `x-telnyx-streaming-auth-token` WebSocket header").
    Brisvox should compare it in constant time and refuse the upgrade (HTTP 401/403) on mismatch. Never log it.
  - `stream_auth_token` is listed on the Call Control `streaming_start` reference and on the `dial` (`POST /v2/calls`)
    reference (first ~100k of 168k characters of the dial page read; the stream_* parameters are all in that part). It is
    NOT listed in the `answer` request parameters (whole page, 129k characters, read in two fetches on 2026-10-08), not in
    the media-streaming guide, and not a TeXML `<Stream>` attribute. So with `answer` or TeXML you cannot set it; start the
    stream with `streaming_start` after answering, or authenticate with a secret in the URL path or a custom parameter
    (see below). Exact upgrade-header casing: UNVERIFIED (spec writes it lower case).
  - `custom_parameters` (streaming_start: array of `{name, value}`; TeXML: `<Parameter name= value=>`) arrive
    in `start.custom_parameters` (a map of strings). They are part of a WS message, not the upgrade, and not
    signed; treat as untrusted unless the token/URL secret validated. Top-level `custom_parameters` was not found on
    `answer` or in the part of `dial` that was read (UNVERIFIED there; it exists on `streaming_start`).
  - Network allowlist: Telnyx does not publish a separate stream source-IP list. The IP-whitelisting page says
    for the webhook CIDRs: "These ranges also apply to WebSocket stream connections." Webhook CIDRs: CH1
    192.76.120.128/29, DC2 192.76.120.136/29, SV1 192.76.120.144/29, LD6 185.246.41.0/29, FR5 185.246.41.8/29,
    AM6 185.246.41.16/29, SY1 103.115.244.0/29, SG1 103.115.244.8/29; webhooks use TCP 443. The list may change
    (the page does not say how often): fetch it again before pinning (UNVERIFIED that it is complete for WS).
  - Webhooks (not the WS) are signed: headers `telnyx-signature-ed25519` and `telnyx-timestamp`
    (stated on the TeXML stream callback reference page; that page notes the headers are compatible with the Standard
    Webhooks spec and can be mapped to `webhook-timestamp`/`webhook-signature`). The algorithm being Ed25519 is inferred
    from the header name; the page does not describe verification steps (UNVERIFIED). Only relevant if brisvox ever serves
    the Call Control webhook; it does not (the user's HTTP app does). Crate/verification details not researched.
- Endpoint(s) / regions:
  - Brisvox hosts a `wss://` endpoint (the spec's server is `wss`, host/path chosen by the customer, address `/`).
    TLS certificate requirements, supported TLS versions, accepted ports and whether `ws://` is accepted: NOT
    stated (UNVERIFIED; use a publicly trusted certificate on 443). Query strings in `stream_url`: not
    forbidden in the docs seen (Twilio forbids them); UNVERIFIED, so prefer path secret + parameters.
  - Regions: no region selection for streaming found (UNVERIFIED). Telnyx source regions are the webhook CIDRs above.
  - Prerequisite (third-party integration guide found by search, not Telnyx docs, so UNVERIFIED; not re-checked in the
    2026-10-08 fact check): a Call Control app with a webhook URL is needed for programmable calls; a plain number
    connection is not enough.
- How a call is set up to stream. Four ways (all documented):
  1. Call Control `dial` (`POST /v2/calls`) with `stream_url`, `stream_track`, optionally
     `stream_bidirectional_mode`, `stream_bidirectional_codec` (guide's examples). The `dial` reference also lists
     `stream_codec`, `stream_bidirectional_target_legs`, `stream_bidirectional_sampling_rate`, `stream_auth_token` and
     `stream_establish_before_call_originate` (boolean, default false; meaning not read: UNVERIFIED).
  2. Call Control `answer` (`POST /v2/calls/{call_control_id}/actions/answer`) with the same `stream_*` fields
     (answer reference lists `stream_url`, `stream_track`, `stream_codec`, `stream_bidirectional_mode`,
     `stream_bidirectional_codec`, `stream_bidirectional_target_legs`; it does NOT list `stream_bidirectional_sampling_rate`
     or `stream_auth_token`, whole page read). Setting `stream_url` also triggers `streaming.started` /
     `streaming.stopped` / `streaming.failed` webhooks (`streaming.failed` is not named in the pages re-read on
     2026-10-08; the guide and streaming-stop page name `streaming.started` and `streaming.stopped`: `streaming.failed`
     UNVERIFIED).
  3. Call Control `streaming_start` on a live call (`POST https://api.telnyx.com/v2/calls/{call_control_id}/actions/streaming_start`).
  4. TeXML: `<Start><Stream>` (asynchronous, call flow continues) or `<Connect><Stream>` (synchronous: the call flow
     waits until streaming stops, then the instructions after `<Connect>` run; the Connect page says "The following
     instructions will be executed when the service is stopped"; it has no XML example, so the `<Connect><Stream ...>`
     snippet further below is derived, not a doc sample). TeXML REST also
     has `POST /v2/texml/Accounts/{account_sid}/Calls/{call_sid}/Streams.json` (form-encoded `Url`, `Track`, `Name`,
     `BidirectionalMode`, `BidirectionalCodec` (PCMU|PCMA|G722 there), `StatusCallback`, `StatusCallbackMethod`).
  - Only ONE streaming/fork operation per call ("Currently only one streaming/fork operation is supported per
    call"); a media-forking request "will be stopped and replaced by a RTP connection" (the WS stream is replaced).
    The `streaming_start` reference shows generic 422 examples (not stream-specific): invalid call control ID (90015),
    call already ended (90018), call not answered yet (90034), "Media Streaming is used" (90045, detail "This command
    can't be issued when media streaming is used"), plus 10004/10032/10033 validation errors.
  - Brisvox example (derived, not a doc sample; JSON key names are from the `streaming_start` reference):
    ```json
    POST https://api.telnyx.com/v2/calls/{call_control_id}/actions/streaming_start
    Authorization: Bearer <API key>
    {
      "stream_url": "wss://agent.example.com/telnyx/stream",
      "stream_track": "inbound_track",
      "stream_codec": "PCMU",
      "stream_bidirectional_mode": "rtp",
      "stream_bidirectional_codec": "PCMU",
      "stream_bidirectional_target_legs": "opposite",
      "stream_bidirectional_sampling_rate": 8000,
      "stream_auth_token": "<random secret>",
      "custom_parameters": [ { "name": "agent", "value": "support-tr" } ],
      "client_state": "<base64>",
      "command_id": "<unique>"
    }
    ```
    The same fields exist as top-level `stream_*` fields on `dial` (with `stream_auth_token`) and, except
    `stream_bidirectional_sampling_rate`/`stream_auth_token`, on `answer`. TeXML equivalent (derived; attributes from the
    TeXML `<Stream>` page; no bidirectional XML example exists in the docs):
    ```xml
    <Response>
      <Connect>
        <Stream url="wss://agent.example.com/telnyx/stream" bidirectionalMode="rtp" bidirectionalCodec="PCMU"
                bidirectionalSamplingRate="8000" enableReconnect="false">
          <Parameter name="agent" value="support-tr" />
        </Stream>
      </Connect>
    </Response>
    ```
  - `streaming_start` parameters (exact, reference page; no field is marked required): `stream_url` (string, e.g. `wss://www.example.com/websocket`);
    `stream_track` (`inbound_track` default | `outbound_track` | `both_tracks`); `stream_codec` (`PCMU`|`PCMA`|`G722`|`OPUS`|
    `AMR-WB`|`L16`|`default`, default `default`; "When set to 'default' or when transcoding is not possible, the codec
    from the call will be used"); `stream_bidirectional_mode` (`mp3` DEFAULT | `rtp`); `stream_bidirectional_codec`
    (`PCMU` default | `PCMA`|`G722`|`OPUS`|`AMR-WB`|`L16`; used only with mode `rtp`; case sensitive);
    `stream_bidirectional_target_legs` (`both`|`self`|`opposite`, default `opposite`); `stream_bidirectional_sampling_rate`
    (integer `8000` default | `16000` | `22050` | `24000` | `48000`); `enable_dialogflow` (bool, default false), `dialogflow_config`
    (`analyze_sentiment`, `partial_automated_agent_reply`, both bool default false);
    `client_state` (base64); `command_id` (dedupe per call_control_id); `custom_parameters`; `stream_auth_token` (max 4000).
    Response 200 `{"data":{"result":"ok"}}`; 422/default `errors[]` with `code`, `title`, `detail`, `source`.
  - TeXML `<Stream>` attributes (exact): `url` (required), `track`, `name`, `codec` (`PCMU`|`PCMA`|`G722`|`OPUS`|`AMR-WB`|
    `default`; "Currently, transcoding is only supported between PCMU and PCMA codecs"), `bidirectionalMode` (`mp3` default|`rtp`),
    `bidirectionalCodec` (`PCMU` default|`PCMA`|`G722`|`OPUS`|`AMR-WB`), `bidirectionalSamplingRate` (`8000` default|`16000`|
    `24000`), `statusCallback`, `statusCallbackMethod` (`GET`|`POST`, default `POST`), `enableReconnect` (default `true`).
    Child `<Parameter name= value=>`. Status callbacks: `stream-started`, `stream-stopped`, `stream-failed`.
    Doc examples on the page use `<Start><Stream url="wss://yourdomain.com/stream" track="both_tracks" />` and a
    `statusCallback` + `<Parameter>` variant.
    Docs discrepancy: Call Control allows rates `22050`/`48000`, TeXML only `8000/16000/24000`, and the TeXML `<Stream>`
    codec list has no `L16`; use Call Control for L16.
- Protocol (messages in/out, with field names). All frames are JSON text frames (binary frames not mentioned:
  UNVERIFIED). Field names are snake_case; stream id key is `stream_id` (a UUID), not `streamSid`.
  - Telnyx -> server, in order (spec "Connection Flow"): `connected`, `start`, then `media` (repeating) and
    optionally `dtmf` / `mark` / `error`, finally `stop`.
    ```json
    { "event": "connected", "version": "1.0.0" }
    ```
    With a configured token:
    ```json
    { "event": "connected", "connected": { "x-telnyx-streaming-auth-token": "3c8b06a4-94a8-4d03-9fd1-941bc3f7166a" }, "version": "1.0.0" }
    ```
    ```json
    {
      "event": "start",
      "sequence_number": "1",
      "stream_id": "32DE0DEA-53CB-4B21-89A4-9E1819C043BC",
      "start": {
        "user_id": "3E6F995F-85F7-4705-9741-53B116D28237",
        "call_control_id": "v2:T02llQxIyaRkhfRKxgAP8nY511EhFLizdvdUKJiSw8d6A9BborherQ",
        "call_session_id": "ff55a038-6f5d-11ef-9692-02420aeffb1f",
        "from": "+13122010094",
        "to": "+13122123456",
        "tags": [ "TAG1", "TAG2" ],
        "client_state": "aGF2ZSBhIG5pY2UgZGF5ID1d",
        "custom_parameters": { "customer_id": "customer_123" },
        "media_format": { "encoding": "PCMU", "sample_rate": 8000, "channels": 1 }
      }
    }
    ```
    ```json
    {
      "event": "media",
      "sequence_number": "4",
      "stream_id": "32DE0DEA-53CB-4B21-89A4-9E1819C043BC",
      "media": { "track": "inbound", "chunk": "2", "timestamp": "5", "payload": "no+JhoaJjpzSHxAKBgYJDhtEopGKh4aIjZm7JhILBwYIDRg1qZSLh4aIjJev" }
    }
    ```
    ```json
    { "event": "dtmf", "stream_id": "32DE0DEA-53CB-4B21-89A4-9E1819C043BC", "occurred_at": "2025-06-05T08:54:19.698408Z",
      "sequence_number": "5", "dtmf": { "digit": "1" } }
    ```
    ```json
    { "event": "mark", "sequence_number": "5", "stream_id": "32DE0DEA-53CB-4B21-89A4-9E1819C043BC", "mark": { "name": "some_mark_name" } }
    ```
    ```json
    { "event": "stop", "sequence_number": "5", "stream_id": "32DE0DEA-53CB-4B21-89A4-9E1819C043BC",
      "stop": { "user_id": "3E6F995F-85F7-4705-9741-53B116D28237", "call_control_id": "v2:T02llQxIyaRkhfRKxgAP8nY511EhFLizdvdUKJiSw8d6A9BborherQ" } }
    ```
    ```json
    { "event": "error", "stream_id": "32DE0DEA-53CB-4B21-89A4-9E1819C043BC",
      "payload": { "code": 100004, "title": "invalid_media", "detail": "Media provided was not base64 encoded" } }
    ```
  - Field notes (spec schemas; every Telnyx-sent object has `additionalProperties: false`, so unknown fields
    are not expected, but ignore them anyway for forward compatibility):
    - `connected`: required `event`, `version` (string, protocol version, example `1.0.0`); optional `connected`
      object with optional `x-telnyx-streaming-auth-token`. NOTE: `connected` has no `sequence_number`/`stream_id`.
    - `start`: required `event`, `sequence_number`, `stream_id` (UUID), `start`. Required inside `start`: `user_id`
      (UUID), `call_control_id`, `media_format`. Optional: `call_session_id` (UUID), `from`, `to` ("Caller number or SIP
      URI when available" for `from`; "Callee number or SIP URI when available" for `to`), `tags` (strings), `client_state`
      (base64 string), `custom_parameters` (string -> string).
      `media_format.encoding` enum `PCMU`|`PCMA`|`G722`|`OPUS`|`AMR-WB`|`L16`; `sample_rate` integer enum `8000`|`16000`;
      `channels` integer, constant `1`. The spec has no `track` list in `start` (Twilio has `tracks`).
    - `media`: required `event`, `sequence_number`, `stream_id`, `media{track, chunk, timestamp, payload}`.
      `track` enum `inbound`|`outbound` ("Call leg track that produced the media frame"); `chunk` string, "Per-stream media
      chunk number. Use to reorder media frames if needed."; `timestamp` string, "RTP timestamp associated with this media
      frame." (so NOT milliseconds like Twilio; units/clock rate, wrap-around and relation to ms: UNVERIFIED; the example
      `"5"` next to chunk `"2"` can't be interpreted); `payload` base64 "RTP payload without RTP headers".
    - `sequence_number`: string, "Monotonic sequence number for frames sent by Telnyx on this WebSocket connection."
      `connected` and `error` frames carry none.
    - TYPES: `sequence_number`, `chunk`, `timestamp` are JSON STRINGS in schema and examples; `sample_rate`, `channels`
      and error `code` are numbers. Parse strings; accept numbers too, defensively (as for Twilio).
    - `dtmf`: required `event`, `sequence_number`, `stream_id`, `dtmf.digit`; optional `occurred_at` (date-time, "Timestamp
      captured by the telephony engine. Use this to order DTMF events if they arrive out of order."). Digit enum:
      `0-9`, `*`, `#`, `A`, `B`, `C`, `D`. No `track` field (Twilio has `dtmf.track`).
    - `stop`: sent "when Telnyx stops media streaming for the call" (spec). The guide says it is sent when the call ends,
      after the `streaming.stopped` webhook; whether it is also sent when the stream is stopped via REST while the call stays
      up: not stated in the pages (UNVERIFIED). Carries `stop.user_id` and `stop.call_control_id`.
    - `error`: required `event`, `stream_id`, `payload{code, title, detail}`; code/title enum: `100002 unknown_error`,
      `100003 malformed_frame`, `100004 invalid_media` (example detail "Media provided was not base64 encoded"), `100005 rate_limit_reached`
      (guide: "Too many requests"). Whether the stream/connection survives an error frame: not stated (UNVERIFIED).
  - Ordering: "The order of events is not guaranteed and the chunk number can be used to reorder the events." (guide);
    the spec says to use `media.chunk`, `media.timestamp` and `sequence_number`. This is a real difference from Twilio:
    brisvox needs a small bounded reorder buffer keyed on `media.chunk` (and tolerate gaps), and DTMF should be ordered
    by `occurred_at`. How far out of order frames get, and the reorder window to use: UNVERIFIED (measure live).
    Whether the `start` frame can arrive after a `media` frame: not stated.
  - Server -> Telnyx (bidirectional; client schemas also have `additionalProperties: false`):
    ```json
    { "event": "media", "media": { "payload": "eW91ciBiYXNlNjQgZW5jb2RlZCBhdWRpbyBwYXlsb2Fk" } }
    ```
    ```json
    { "event": "mark", "mark": { "name": "some_mark_name" } }
    ```
    ```json
    { "event": "clear" }
    ```
    These carry NO `stream_id` (Twilio requires `streamSid` on every message). Because the schemas forbid extra
    properties, sending a `stream_id` may be rejected as `malformed_frame` (100003): inference from the schema, not a
    doc sentence, UNVERIFIED. Pipecat's official Telnyx serializer sends exactly `{"event":"media","media":{"payload":...}}`
    and `{"event":"clear"}` with no stream id (source read 2026-10-08; it stores `stream_id` but never sends it), which is
    consistent. There is no server->Telnyx `stop`, and no documented way to send DTMF or an error to Telnyx over the socket.
- Outbound audio format and chunking:
  - Bidirectional mode `rtp`: payload = base64 of the RTP payload (no RTP header) in the codec chosen by
    `stream_bidirectional_codec` (default PCMU). Guide table of codecs and rates: PCMU 8 kHz (default), PCMA 8 kHz, G722 8 kHz,
    OPUS 8 and 16 kHz, AMR-WB 8 and 16 kHz, L16 16 kHz. Verbatim warnings: when audio is sent in a different encoding than
    the call's "it will be transcoded, which may cause a degradation in quality"; the guide recommends L16 for AI voice
    agents (lower latency, no transcoding overhead). So the call's own codec (negotiated by the carrier/SIP leg) matters;
    the `start.media_format` tells the INBOUND encoding actually streamed.
  - Chunk size: "Provided chunks of audio can be in a size of 20 milliseconds to 30 seconds" (RTP mode). So Telnyx
    accepts large chunks, unlike a fixed 20 ms pacing; brisvox can still use 20 ms frames (160 bytes PCMU at 8 kHz).
    Minimum is 20 ms: smaller chunks are probably rejected or merged (UNVERIFIED; do not send < 20 ms).
  - L16 byte order (big/little endian), and whether it is raw linear PCM16: the docs say "L16" and "linear PCM" only;
    RFC 3551 L16 is network byte order, but Telnyx does not state it: UNVERIFIED (test live before using L16).
  - Rate limit: the guide says "Media payloads can only be submitted once per second." and this sentence sits under the
    guide's "Sending media files" subsection (MP3 files) of "Bidirectional media streaming", not under "Sending RTP
    stream". The spec adds MP3 frames are "played in submission order, with a one-message-per-second rate limit"; its RTP
    sentence ("RTP media chunks sent back to Telnyx can contain 20 milliseconds to 30 seconds of audio") carries no rate
    limit. So per the docs the once-per-second limit is for MP3 file payloads; that it does not apply to RTP-mode `media`
    frames (50/s at 20 ms) is an inference, not a doc sentence (UNVERIFIED): confirm live and watch for error 100005.
  - MP3 mode (the DEFAULT mode when `stream_bidirectional_mode` is not set): `media.payload` is a base64 MP3 FILE, queued
    and played in order. Not suitable for streaming TTS: always set `stream_bidirectional_mode` to `rtp`. A stream
    started without it would take brisvox PCM/G.711 frames as broken MP3 (UNVERIFIED consequence). (Note the guide's
    dial example for rtp also shows `rtp` explicitly.)
  - `stream_bidirectional_target_legs`: `opposite` (default), `self`, `both` "Specifies which call legs should receive the
    bidirectional stream audio". For an inbound call answered by brisvox the stream is on the caller's leg; the
    meaning of `opposite` for a single-leg inbound call is not explained in the pages read: UNVERIFIED. Test whether the
    caller hears the audio with defaults, else set `self`.
  - Tracks: `stream_track` selects `inbound_track` (caller audio; default), `outbound_track` (audio Telnyx plays to the
    caller, i.e. includes brisvox's own bidirectional audio, UNVERIFIED), or `both_tracks`. For the agent use
    `inbound_track` only; `media.track` is `inbound` for that. Drop frames whose `track` is not `inbound`.
  - Buffer capacity (Twilio: 10 minutes): not documented for Telnyx (UNVERIFIED). Whether brisvox may send ahead of real
    time and rely on `clear`: implied by the `clear`/`mark` text below but the queue size limit is unknown.
- Mark semantics (guide + spec):
  - Server sends `mark` after the `media` to track. Telnyx returns `{"event":"mark",...,"mark":{"name":..}}` "when the media
    immediately preceding a submitted mark finishes playing. If no audio is playing or queued, the mark is returned
    immediately." (spec). Guide: "Submitting marks when no audio is played or queued will result in them being immediately
    sent back." This is stated explicitly for Telnyx (Twilio only implies it).
  - `clear`: guide "Sending a clear message will immediately stop the media playing on the stream and clear the media queue."
    Spec: "Stops the current media playback and clears queued media. Queued marks are sent back by Telnyx." So the pattern
    is the same as Twilio: marks for cleared audio are echoed with the same JSON as played marks; brisvox must track
    locally that it sent `clear` and drop the echoes (exactly as the Twilio adapter forgets in-flight marks before
    sending `clear`). Note the improvement over Twilio: `clear` "immediately stops the media playing", i.e. also the
    currently playing chunk (Twilio says it empties the buffer). Residual audio already in the phone network: UNVERIFIED.
  - Marks carry no timestamp or played-sample count; the mark name is a free string ("Customer-defined mark name"; length
    and charset limits not documented, UNVERIFIED; reuse the Twilio scheme `e<n>` / `p<n>`). Whether two in-flight marks
    may share a name: not stated (use unique names).
  - Whether a mark echo arrives in order relative to media/dtmf frames: no ordering guarantee exists for Telnyx frames in
    general (see Ordering); use mark names, not arrival order, to match.
  - Whether marks work in MP3 mode: the guide says they track when media "ends on the stream"; not stated which modes. Use
    `rtp` mode.
  - Playback tracking design for played-text truncation is identical to Twilio's (progress marks every ~200 ms plus utterance-end marks;
    heard = confirmed marks + estimate of the unconfirmed tail); see twilio.md "Marks, clear and played-text tracking".
- Interruption (barge-in): send `{"event":"clear"}` the moment the turn epoch advances, before any new-epoch audio; stop
  sending old-epoch `media`; drop mark echoes that follow the clear (they refer to cleared audio). Whether media sent right
  after `clear` plays normally: implied (queue emptied), not stated (UNVERIFIED).
- Keepalive / timeouts / reconnect:
  - No ping interval, idle timeout, maximum stream duration or close codes are documented (UNVERIFIED). Brisvox applies its
    own timeouts (time to first `start` after upgrade, stall timer on inbound `media`, ping/pong from our side as in the
    generic WebSocket server). Inbound `media` cadence and frame duration: not documented (UNVERIFIED; the spec says a `media`
    frame carries one RTP payload, plausibly 20 ms).
  - Reconnect: TeXML `<Stream enableReconnect>` (default `true`): "Whether the platform should automatically attempt to
    reconnect the WebSocket stream if disconnected." The TeXML page suggests `enableReconnect="false"` "to end the stream when
    the WebSocket disconnects" (page: end the stream immediately on disconnection). If reconnect is on and brisvox closes the
    socket (e.g. hang-up attempt) Telnyx may reconnect and
    deliver a fresh `connected`/`start` for the same call; for the engine this means the same `call_control_id` could reach
    the server twice (inference, UNVERIFIED). Brisvox must (a) recommend `enableReconnect="false"` for `<Stream>`, (b) key sessions by
    `call_control_id` and refuse/close a second connection for a live or ended call. Whether Call Control streams have an
    equivalent setting (none in `streaming_start`) and whether `stream_id` stays the same after a reconnect: UNVERIFIED.
  - `streaming.failed` webhook (and TeXML `stream-failed`) = "Stream failed to start or was interrupted". The TeXML
    `stream-failed` wording ("failed to start or was interrupted") is confirmed on the TeXML stream page; the Call Control
    `streaming.failed` webhook name was not found on the pages re-read on 2026-10-08 (UNVERIFIED).
  - Closing the WS from the server: the guide does NOT say closing the WebSocket stops the stream or ends the call (UNVERIFIED).
    For `<Connect><Stream>` the next verbs run after the service stops; whether a server-initiated close counts as "stops"
    is not stated.
- Ending the call / stopping the stream (differs from Twilio):
  - There is no server->Telnyx `stop` message and (unlike Twilio) closing the socket is not documented as a hang-up.
    To hang up, call REST `POST /v2/calls/{call_control_id}/actions/hangup` (body optional fields `client_state`, `command_id`,
    `custom_headers` (SIP BYE headers); 200 `{"data":{"result":"ok"}}`; 422 codes `90015` invalid call control ID, `90018`
    call already ended; webhooks `call.hangup`, `call.recording.saved`). Pipecat's Telnyx serializer does exactly this
    (`auto_hang_up` default on, needs `call_control_id` and API key) on its `EndFrame`/`CancelFrame`.
  - To stop only the stream: `POST /v2/calls/{call_control_id}/actions/streaming_stop` with optional `stream_id` (UUID; if
    omitted "stops all streams associated with the call_control_id"), `client_state`, `command_id` (the request body object
    itself is marked required, all its fields optional, so send `{}`); sends the
    `streaming.stopped` webhook; 422 codes 90015/90018/90034. The TeXML equivalent ("update streaming") "moves the stream to a completed state and closes
    its WebSockets" (TeXML streams overview page).
  - Consequence for brisvox: the engine's `Hangup` needs a REST client (API key + `call_control_id` from `start`), supplied by the
    host app; a transport-level hang-up by closing the socket alone may leave the call up (UNVERIFIED either way). Provide
    a pluggable hang-up callback (e.g. a `Hangup` hook that calls the REST API) in the adapter config, and still close the
    socket after.
  - As with Twilio, to play a goodbye: send audio, then an utterance-end mark, wait for the echoed mark (own timeout), then hang up.
- Webhooks around the stream (Call Control; field names from the guide as key lists): `streaming.started` and
  `streaming.stopped` have top-level `event_type`, `id`, `occurred_at`, `payload`, `record_type`; `payload` has `call_control_id`,
  `stream_url` (the top-level key list was not re-confirmed on 2026-10-08, only the `payload` keys: UNVERIFIED). `call.answered` carries `stream_url`, `stream_track` (not re-confirmed: UNVERIFIED). TeXML `statusCallback` payload fields: not found (the stream
  callback reference page had no schema: UNVERIFIED). These are HTTP callbacks to the user's app, not to brisvox core.
- Timestamps / interim results / tool-call deltas: not applicable. Relevant timing data: inbound `media.timestamp` is an RTP
  timestamp string (not ms), `chunk` counter (gaps = lost or reordered inbound audio), `sequence_number` counts every
  Telnyx-sent frame; outbound `media` has no timestamp or chunk field.
- Concurrency / rate limits: one streaming/fork operation per call; one bidirectional RTP stream per call (guide: "The
  number of bidirectional RTP streams per call is limited to 1."); media chunk 20 ms to 30 s; "Media payloads can only be
  submitted once per second" (guide places it under media-file/MP3 sending, see above); error 100005 `rate_limit_reached`. Per-account concurrent stream
  limits, pricing and messages-per-second for RTP: NOT found (UNVERIFIED).
- Turkish support: not applicable to the transport (language-agnostic audio). Telephony quality note as in twilio.md: PCMU/PCMA
  8 kHz is narrowband; L16/G722/OPUS 16 kHz (if the call leg negotiates it) gives wideband audio that Deepgram/Cartesia can use
  directly. Whether a Turkish PSTN call ever negotiates a wideband codec: UNVERIFIED (carrier dependent).

## Differences from Twilio (summary)
(The Twilio column was not re-checked in the 2026-10-08 fact check; see twilio.md for its sources.)

| Topic | Twilio Media Streams | Telnyx Media Streaming |
|---|---|---|
| Start of call | TwiML `<Connect><Stream url>` | Call Control `dial`/`answer`/`streaming_start` with `stream_url`; or TeXML `<Connect><Stream>` / `<Start><Stream>` |
| Bidirectional enable | implicit with `<Connect>` | explicit `stream_bidirectional_mode: rtp` (default is `mp3`) |
| Key naming | camelCase (`streamSid`, `callSid`, `mediaFormat`, `sequenceNumber`) | snake_case (`stream_id`, `call_control_id`, `media_format`, `sequence_number`) |
| Stream id | `streamSid` (`MZ...`), required in every server message | `stream_id` UUID in Telnyx frames; NOT sent by the server |
| `connected` | has `protocol`, `version` | has `version` only (+ optional token) |
| `start` | `streamSid`, `callSid`, `accountSid`, `tracks`, `mediaFormat`, `customParameters` | `stream_id`, `start{user_id, call_control_id, call_session_id, from, to, tags, client_state, custom_parameters, media_format}` |
| Codec | always `audio/x-mulaw` 8000 mono | `PCMU`/`PCMA`/`G722`/`OPUS`/`AMR-WB`/`L16`; 8000 or 16000 in `start`; bidirectional codec set separately (default PCMU) |
| `media.timestamp` | ms since stream start | "RTP timestamp" (unit unverified) |
| Frame order | in order | NOT guaranteed; reorder by `chunk` |
| DTMF | `dtmf.track`, `dtmf.digit` | `dtmf.digit`, `occurred_at` (no track) |
| Mark on empty queue | implied | explicit: returned immediately |
| Clear | empties buffer, returns marks | "immediately stop[s] the media playing" and clears queue, returns marks |
| Chunk size | any | 20 ms to 30 s (RTP) |
| Send-rate rule | none stated (10-minute buffer cap) | "once per second" for media-file (MP3) payloads per the guide; RTP scope unverified; `rate_limit_reached` |
| Errors to server | none; only logs | `error` frame codes 100002..100005 |
| Hang-up | close the WS | REST `hangup` (WS close not documented as hang-up) |
| `stop` from server | not accepted | not accepted |
| Request authenticity | `X-Twilio-Signature` HMAC-SHA1 over URL | none on WS; optional `stream_auth_token` header/field (streaming_start/dial only); webhooks signed (`telnyx-signature-ed25519`) |
| Reconnect | none documented | TeXML `enableReconnect` default `true` |
| Custom params | `<Parameter>` (<500 chars name+value) | `custom_parameters` / `<Parameter>` (no limits documented) |

## Mapping to the brisvox transport trait (proposal reusing the Twilio adapter structure, check the real trait when implementing)
- Inbound: parse by `event`; `connected` (check token header/field if configured), `start` -> `CallInfo` (`call_id` =
  `call_control_id`; metadata `transport=telnyx`, `stream_id`, `call_control_id`, `call_session_id`, `user_id`, `from`, `to`, `tags`,
  `client_state`, `encoding`, `sample_rate`, `custom.<name>`), `media` (track `inbound`) -> reorder buffer (by `chunk`) ->
  decode by `start.media_format.encoding`: PCMU/PCMA via `brisvox_audio::codec` (g711.md); L16 (16 kHz) as PCM16; G722/OPUS/AMR-WB
  not decodable in-tree (refuse with a clear error, or require a codec in `stream_codec`) -> PCM16 `AudioFrame`. `dtmf` ->
  DTMF event (ordered by `occurred_at`), `mark` -> `MarkAck` or drop (post-clear), `stop` -> hang-up
  (`PeerHangup`), `error` -> fault (code in message), unknown `event` ignored. Drop frames whose `stream_id` differs from `start`.
- Outbound: PCM16 at the stream rate -> encode in the SAME codec set as `stream_bidirectional_codec` (document that the
  host must set `stream_bidirectional_codec` equal to the inbound encoding to avoid transcoding; start-time config is the
  host's job) -> base64 -> `{"event":"media","media":{"payload":..}}`; marks `e<n>`/`p<n>` as in the Twilio adapter; `clear` per rule above.
  The adapter must know the outbound codec: it is NOT announced in `start` (that is the inbound `media_format`). Make it a
  `TelnyxConfig` field (default PCMU 8 kHz) and assert it equals `start.media_format` when inbound==outbound is expected.
- Auth: constant-time compare of `x-telnyx-streaming-auth-token` (upgrade header; fall back to `connected.*`) with the configured
  secret; additional option of a secret path segment. 401/403 on mismatch before the upgrade completes. The token can only be
  set via `streaming_start` or `dial`, not `answer`/TeXML (see Auth), so the path secret is the fallback.
- Hang-up: host-provided async hook (REST `hangup`), plus close frame; see "Ending the call".
- Reuse of the Twilio server infrastructure (call limit, accept queue, ping/idle/write timeouts, mark bookkeeping) is expected; the
  new parts are the reorder buffer, snake_case/UUID parsing, the codec choice and the hang-up hook.
- Gotchas found during implementation (`brisvox_transport::telnyx`, tests with a scripted peer only):
  - The reorder buffer anchors on the first frame it sees. If chunk 2 overtakes chunk 1 at the very start, chunk 1 is
    dropped as late (20 ms). The first chunk number is not documented (UNVERIFIED), so nothing is assumed.
  - A frame arriving while earlier ones are held takes the buffer path even when it is the chunk due (it is released at
    once); `media_reordered` counts those too.
  - Frames may overtake `start` (nothing says they cannot): they are dropped and counted, at most 64, then the
    connection is refused (1008). Tested with a start timeout of 30 s: the 65th frame refuses (`media_before_start == 65`),
    64 frames followed by `start` open the call.
  - Review fixes (verified findings), all by the adapter's own tests with a scripted peer:
    - A stream the HOST stops on purpose (`streaming_stop`, e.g. the retry after a `transfer`/conference command was refused
      with `90045`) must not look like the caller hanging up. Telnyx then closes the socket; whether it sends `stop` first is
      UNVERIFIED (open item 7). Without help the adapter would report a hang-up to the engine (ending its session and the
      tool call in flight) or have the hook REST-hang-up the call being transferred. Additive API:
      `TelnyxServer::stream_stopper()` returns a cloneable `StreamStopper`; `expect_stop(call_control_id)` is called BEFORE
      `streaming_stop` and returns whether the call streams here. For `STOP_EXPECTED_WITHIN` (30 s; a conservative window
      that covers the REST call plus the close, and makes a forgotten announcement harmless) a `stop` frame, a close frame
      or a vanished socket of that call is the host's own doing: no hook call, no hang-up event (the session is kept, what
      the engine sends is discarded), the call key stays claimed (a TeXML reconnect is refused). The session ends when the
      engine ends it, the server shuts down or after `STOPPED_LINGER` (120 s, conservative upper bound for a retried
      transfer; the engine then sees the peer disconnect). Intended caller: the call-control binding's `90045` retry path, which must call
      `expect_stop` before it issues `streaming_stop` (the API is additive; the binding itself is a separate change).
    - `max_control_bytes` (default 16 KiB) bounds control messages only. A `media` frame is bounded by `max_audio_bytes`
      (default 64 KiB; about 32 KiB of G.711, 43 KB of base64) through the websocket limit and the payload check; it used
      to be rejected as "control message too large" (1009, call hung up) above 16 KiB although Telnyx documents no size for
      inbound chunks. Everything that is not `media`, or does not parse, is still bounded by `max_control_bytes`.
  - Header casing of the token is irrelevant to the code (HTTP header names are case-insensitive), so open item 1 only
    concerns whether the header is sent at all; the adapter accepts the token from either place and refuses a wrong
    one from either.
  - DTMF is ordered by `sequence_number` (a per-connection counter shared with every other frame, so it has gaps) after a
    short hold (`reorder_timeout`), not by `occurred_at`; both work, the counter avoids parsing RFC 3339.
  - Not done: L16 (byte order), G722/OPUS/AMR-WB, `stream_track=both_tracks` (chunk numbering across tracks is
    undocumented), a transport-level REST hang-up
    (the host passes a `HangupHook`), per-account stream limits.

## Open items to settle with a live test (needs a Telnyx account, a number, a Call Control app and a public wss URL)
1. Exact header casing of `x-telnyx-streaming-auth-token` on the upgrade; whether `answer`/TeXML have an undocumented way to set it
   (docs: only `streaming_start` and `dial` list `stream_auth_token`).
2. Whether `media` frames in `rtp` mode are subject to the "once per second" rule (expected not; the guide puts the rule under media-file sending), and the behavior on 100005.
3. How out-of-order inbound frames really are (window size for the reorder buffer); unit of `media.timestamp`; `media` cadence.
4. L16 byte order and exact sample format; effect of `stream_bidirectional_sampling_rate` vs codec (PCMU at 16000?) and which sample
   rate `start.media_format` reports for each combination; whether 24000/48000 work with L16/OPUS.
5. Meaning of `stream_bidirectional_target_legs` for a single-leg inbound call (does `opposite` reach the caller?).
6. Residual audio after `clear`; whether marks echoed by `clear` arrive before or after other frames; whether media after `clear` plays at once.
7. Whether closing the WS ends the call, stops the stream, or triggers reconnect (and `enableReconnect` behavior); close codes; keepalive.
8. Whether a `stream_id` in server frames is rejected (`malformed_frame`) or ignored; whether extra properties are tolerated.
9. Whether Telnyx plays queued audio after the server closes the socket; `error` frame effect on the connection.
10. Latency Telnyx adds between our `media` and the caller's ear (feeds the LATENCY.md budget).
11. Query strings in `stream_url`; TLS requirements; source IPs actually used (compare against the webhook CIDRs).

## Sources (all fetched 2026-10-08; re-fetched and fact-checked 2026-10-08)
- https://developers.telnyx.com/docs/voice/programmable-voice/media-streaming (and .md; limits, codec table, mark/clear rules, error table, webhooks, dtmf, ordering)
- https://developers.telnyx.com/api-reference/websockets/stream-call-media-over-websocket (and .md; AsyncAPI summary, field descriptions)
- https://telnyx-openapi-ng.s3.us-east-1.amazonaws.com/media-streaming/media-streaming.yml (AsyncAPI 3.0.0 v2.0.0 raw; all JSON examples and schemas; token header)
- https://developers.telnyx.com/api-reference/call-commands/streaming-start.md (parameters, enums, defaults, response)
- https://developers.telnyx.com/api-reference/call-commands/streaming-stop.md (stream_id optional, streaming.stopped)
- https://developers.telnyx.com/api-reference/call-commands/answer-call.md (stream_* parameters; whole page read in two fetches, 129k characters)
- https://developers.telnyx.com/api-reference/call-commands/dial.md (stream_* parameters incl. `stream_auth_token`, `stream_bidirectional_sampling_rate`; first 100k of 168k characters read)
- https://developers.telnyx.com/api-reference/call-commands/hangup-call.md (hangup endpoint and error codes)
- https://developers.telnyx.com/docs/voice/programmable-voice/texml-verbs/stream (and .md; attributes, enableReconnect, Parameter, callbacks)
- https://developers.telnyx.com/docs/voice/programmable-voice/texml-verbs/connect.md (`<Connect>` synchronous; instructions after run when the service stops; no Stream XML example)
- https://developers.telnyx.com/docs/voice/texml/rest-api/streams.md and https://developers.telnyx.com/api-reference/texml-rest-commands/start-streaming-media-from-a-call.md (TeXML REST start/stop)
- https://developers.telnyx.com/api-reference/callbacks/texml-stream.md (webhook signing headers `telnyx-timestamp`, `telnyx-signature-ed25519`; no payload schema)
- https://developers.telnyx.com/docs/voice/sip-trunking/network-configuration/ip-whitelisting.md (webhook CIDRs apply to WebSocket streams; TCP 443)
- https://raw.githubusercontent.com/pipecat-ai/pipecat/main/src/pipecat/serializers/telnyx.py (official Pipecat serializer source: media/clear without stream id; REST auto hang-up)
- https://docs.pipecat.ai/api-reference/server/services/serializers/telnyx.md (serializer parameters; auto_hang_up)
- Web search summaries (not primary text, used only where labeled UNVERIFIED): Call Control app prerequisite from a third-party guide; the IP-allowlist answer was cross-checked against the fetched IP page.
- Not fetched / not researched: crates.io (no new crate is needed for this transport: JSON, base64 and tungstenite are already in the tree; Ed25519 webhook verification crate selection is out of scope since brisvox core does not serve the webhook), Telnyx pricing, per-account stream limits, Telnyx sample repos (team-telnyx on GitHub), the sip.telnyx.com `voice.json` file.

Fact-checked 2026-10-08: 62 claims checked, 9 corrected. Corrections: (1) `stream_auth_token` also exists on `dial` (`POST /v2/calls`), not only `streaming_start`; it is absent from `answer` (whole page read) and TeXML, so the old "unverified whether settable via dial" is resolved for dial/answer/TeXML; (2) `dial` also lists `stream_bidirectional_sampling_rate` and `stream_establish_before_call_originate`, while `answer` does not list the sampling rate (was UNVERIFIED); (3) the streaming_start description quote for `stream_auth_token` corrected to "An authentication token to be sent as part of the WebSocket connection"; (4) "once per second" is under the guide's "Sending media files" (MP3) subsection, not RTP; scope for RTP downgraded from "unclear" to "inference, UNVERIFIED"; (5) the claimed TeXML Connect page example (`<Connect><Stream .../></Connect><Say>`) does not exist on the Connect page; removed and the snippet is marked derived; (6) the 422 list under `streaming_start` is generic spec examples (90015, 90018, 90034, 90045, 10004, 10032, 10033), not stream-specific cases; (7) Ed25519 is inferred from the header name `telnyx-signature-ed25519` (page gives no algorithm text), plus a Standard Webhooks compatibility note was added; (8) the guide says `stop` is sent when the call ends after `streaming.stopped`; the "or stream stopped via REST" part is UNVERIFIED; the `streaming_stop` body object is required (fields optional); (9) the Call Control `streaming.failed` webhook name, the webhook top-level key list and `call.answered` fields were not found in the re-read pages and are now marked UNVERIFIED. Still UNVERIFIED after re-check: top-level `custom_parameters` on `dial`/`answer`, the Call Control app prerequisite (third-party source, not re-checked), the Twilio comparison column (belongs to twilio.md), and all behavioral items in "Open items".
