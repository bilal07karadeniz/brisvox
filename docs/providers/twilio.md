# Twilio Media Streams (bidirectional) — telephony transport

Scope: Twilio Media Streams (mu-law 8k; interruption sends the buffer-clear message; playback tracking via marks
feeds played-text truncation; TwiML example). Related: `docs/providers/g711.md` (codec), `docs/providers/call-control.md` (DTMF, transfer).
Researched: 2026-10-08. Every fact below was read from the pages listed under Sources on that
date. Anything not confirmed is marked UNVERIFIED. This is a transport, not an STT/LLM/TTS
provider: Twilio is the WebSocket CLIENT, brisvox is the WebSocket SERVER.

- Docs: https://www.twilio.com/docs/voice/media-streams/websocket-messages (fetched 2026-10-08); full source list at the bottom.
  JSON examples below come from the `.md` rendition of that page (`.../websocket-messages.md`), where
  the plain page fetch would only summarize them. Values like `MZXXXX...` are the docs' placeholders.
- Auth / trust:
  - There is no API key for the stream. Twilio connects to the `wss://` URL in the TwiML. The only
    documented authenticity check is the `X-Twilio-Signature` header on the WebSocket upgrade request
    (see "Request validation" below).
  - Brisvox: the TwiML is served by the user's HTTP webhook (not by brisvox core); the WS endpoint
    must validate the signature with the account's primary Auth Token. Never log the token.
- Endpoint(s) / regions:
  - Brisvox hosts a `wss://` endpoint. `wss` is the only supported protocol and the `<Stream url>`
    does not support query string parameters (use `<Parameter>` instead); a relative or an absolute URL
    is accepted (stream TwiML page).
    Error 31923 "Malformed URL" = URL scheme malformed; 31920 "Handshake Error" = server did not answer the
    upgrade with HTTP 101 (wrong path, no WebSocket support, query string in the URL).
  - Twilio must reach the server on TCP 443 "from any public IP address"; the firewall page lists
    NO IP ranges, hostnames or other ports. TLS version/certificate requirements: not stated
    (UNVERIFIED; use a normal publicly trusted certificate). Terminate TLS in front of brisvox
    (proxy) or in-process; behind a proxy the URL used for signature validation must be the public one.
  - Regions: Media Streams is available in Ireland (`IE1`) and Australia (`AU1`); the default region
    is `US1` (media-streams overview). `<Connect><Stream>` is also stated available in IE1/AU1
    (connect TwiML page). Non-US setup follows separate Twilio guides (not fetched: UNVERIFIED details).
- TwiML to start a bidirectional stream (stream TwiML page; `<Connect><Stream>`):
  ```xml
  <Response>
    <Connect>
      <Stream url="wss://example.com/audiostream" />
    </Connect>
    <Say>This TwiML instruction is unreachable unless the Stream is ended by your WebSocket server.</Say>
  </Response>
  ```
  - With custom parameters (docs example is for `<Start><Stream>`; the same `<Parameter>` noun nests
    in `<Connect><Stream>`: the page says `<Parameter>` is nested in `<Stream>` and the Start message
    carries the values; a `<Connect>` example with `<Parameter>` is not printed: UNVERIFIED but implied):
    ```xml
    <Response>
      <Start>
        <Stream url="wss://mystream.ngrok.io/example">
          <Parameter name="FirstName" value="Jane" />
          <Parameter name="LastName" value="Doe" />
        </Stream>
      </Start>
    </Response>
    ```
    Brisvox example (derived, not a doc sample):
    ```xml
    <Response>
      <Connect>
        <Stream url="wss://agent.example.com/twilio/stream" name="brisvox">
          <Parameter name="agent" value="support-tr" />
        </Stream>
      </Connect>
    </Response>
    ```
  - `<Stream>` attributes (stream TwiML page): `url` (required), `name` (optional, unique per call),
    `track` (`inbound_track` default | `outbound_track` | `both_tracks`; unidirectional only, a
    bidirectional stream "can receive only `inbound_track`"), `statusCallback` (absolute URL),
    `statusCallbackMethod` (`GET`|`POST`, default `POST`).
  - `<Parameter name= value=>`: the combined length of each `<Parameter>` `name` and `value`
    must be under 500 characters (stream TwiML page). No maximum parameter count is stated (UNVERIFIED).
    Values arrive in `start.customParameters` (all as strings in the doc example).
  - `<Connect><Stream>` blocks: Twilio does not execute later TwiML until the server closes the
    WebSocket (stream TwiML page; overview page says the same). With no further verb the call ends
    (general `<Connect>` page: "If there is no further verb, Twilio will end the phone call", stated for
    the case without an `action` URL). The Stream resource (REST) cannot start a bidirectional stream.
- Protocol (messages in/out, with field names). All frames are JSON text frames (binary frames are not
  mentioned: UNVERIFIED).
  - Twilio -> server, in order: `connected`, then `start`, then `media` (repeating) and
    optionally `dtmf`/`mark`, finally `stop`.
    ```json
    { "event": "connected", "protocol": "Call", "version": "1.0.0" }
    ```
    ```json
    {
      "event": "start",
      "sequenceNumber": "1",
      "start": {
        "accountSid": "ACXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
        "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
        "callSid": "CAXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
        "tracks": [ "inbound" ],
        "mediaFormat": { "encoding": "audio/x-mulaw", "sampleRate": 8000, "channels": 1 },
        "customParameters": { "FirstName": "Jane", "LastName": "Doe", "RemoteParty": "Bob" }
      },
      "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
    }
    ```
    ```json
    {
      "event": "media",
      "sequenceNumber": "4",
      "media": { "track": "inbound", "chunk": "2", "timestamp": "5", "payload": "no+JhoaJjpzS..." },
      "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
    }
    ```
    ```json
    {
      "event": "stop",
      "sequenceNumber": "5",
      "stop": { "accountSid": "ACXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX", "callSid": "CAXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX" },
      "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
    }
    ```
    ```json
    { "event": "dtmf", "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX", "sequenceNumber": "5",
      "dtmf": { "track": "inbound_track", "digit": "1" } }
    ```
    ```json
    { "event": "mark", "sequenceNumber": "4", "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
      "mark": { "name": "my label" } }
    ```
  - Field notes (from the messages page):
    - `connected`: `protocol` ("protocol for the connection's lifetime", example `"Call"`), `version`
      (semantic version, example `1.0.0`). First message.
    - `start`: sent once, immediately after `connected`. `start.tracks` array of `inbound`/`outbound`;
      `start.mediaFormat.encoding` is always `audio/x-mulaw`, `sampleRate` always `8000`,
      `channels` always `1`; `start.customParameters` = the `<Parameter>`s; `streamSid` (also repeated at
      top level of every message). `sequenceNumber` starts at 1 and increases per message.
    - `media.track`: `inbound` or `outbound` (one of the page's `media` examples shows `"outbound"`). For
      a bidirectional stream only the inbound track is delivered (stream TwiML page / overview page).
      `media.chunk`: starts at 1, increments per media message. `media.timestamp`: "Presentation
      Timestamp in Milliseconds from the start of the stream". `media.payload`: raw audio, base64.
    - TYPES: in every doc example `sequenceNumber`, `media.chunk` and `media.timestamp` are JSON
      STRINGS (`"1"`, `"2"`, `"5"`), while `sampleRate` and `channels` are numbers. The property
      tables describe them as numbers ("Number used to keep track of message sending order").
      Brisvox parser must accept both string and number for these three fields. (Parsing as integer
      from a string is the safe approach.)
    - `dtmf` (bidirectional only): `dtmf.track` is always `inbound_track` (note: differs from the
      `inbound` value used in `media.track`), `dtmf.digit` the key. Digit alphabet beyond a single
      key (`0-9`, `*`, `#`, `A-D`) is not specified: UNVERIFIED. Outbound DTMF from the server to
      Twilio is NOT supported (overview page: DTMF only inbound, from Twilio toward your media server).
    - `mark` (Twilio -> server): bidirectional only; sent when audio the server sent has finished
      playing (or was cleared), with the `mark.name` the server chose.
    - `stop`: sent when the stream stops or the call ends. The messages page says for bidirectional
      streams "the only way to stop a Stream is to end the call", but the overview page says a
      bidirectional stream can be stopped "by closing the WebSocket connection from your server or by
      ending the Call", and error 31921 (paraphrase) says the close stops a bidirectional Stream.
      Treat server close as stream end.
  - Server -> Twilio (bidirectional streams only, started with `<Connect><Stream>`). Only `media`,
    `mark` and `clear` are accepted (errors 31951/31924); anything else, bad JSON, missing or unexpected
    fields or a `streamSid` that does not match the active stream is logged as "Invalid Message" (31951).
    ```json
    { "event": "media", "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX", "media": { "payload": "a3242sa..." } }
    ```
    ```json
    { "event": "mark", "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX", "mark": { "name": "my label" } }
    ```
    ```json
    { "event": "clear", "streamSid": "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX" }
    ```
    Use the `streamSid` from the `start` message (31951 page: never reuse a SID from another call or stream).
- Outbound audio format and chunking:
  - `media.payload` must be (messages page) "encoded `audio/x-mulaw` with a sample rate of `8000` and
    must be base64 encoded" and "should not contain audio file type header bytes" (i.e. no WAV/AU
    header; raw G.711 mu-law, 8000 Hz, mono, 1 byte per sample = 8000 bytes/s). Error 31924 repeats
    (paraphrase): send base64-encoded `audio/x-mulaw` at 8000 Hz without audio file header bytes.
  - Chunk size: "The audio can be of any size." No required or recommended chunk size or send interval is
    documented (UNVERIFIED claim that 20 ms = 160 bytes frames are ideal: it is arithmetic only,
    160 bytes = 20 ms at 8 kHz mu-law; that is a brisvox choice and matches the 20 ms pacer frame in
    the engine). The same applies to inbound: the docs give no inbound frame duration (UNVERIFIED;
    examples show `timestamp` "5" next to `chunk` "1"/"2", which cannot be used to infer it).
  - Buffering: "The media messages are buffered and played in the order received." So Twilio keeps its
    OWN jitter/playout buffer on its side; brisvox may send ahead of real time, but anything sent ahead
    can only be removed with `clear` (see below) and is invisible to brisvox's pacer unless marks track it.
  - Buffer limit (error 31931 "Media Discarded", a warning logged once per stream): once buffered audio
    exceeds 10 minutes, Twilio stops accepting more audio for that Stream (page wording paraphrased).
    Causes listed: sending `media` faster than it plays, no `mark` tracking. Docs' remedies: pace
    outbound media, send a `mark` after each `media` message and wait for the matching `mark` event
    before sending large amounts of more audio, and `clear` to empty the buffer before replacement audio.
  - Inbound overflow (error 31930 "Buffer Overflow", warning): if limited bandwidth stops the stream from
    keeping up, Twilio discards audio (inbound audio is lost; no reconnect documented).
- Mark semantics:
  - Server sends `mark` after the `media` it wants to track. Twilio returns a `mark` event with the
    same `mark.name` "when the audio ends (or if there is no audio buffered)" (messages page wording).
    The page does not use the word "immediately"; an immediate return for an empty buffer is implied
    (UNVERIFIED as to timing).
  - On `clear`: the page says it "empties all buffered audio and causes any `mark` messages to be
    sent back to your WebSocket server", i.e. Twilio sends back a `mark` for every remaining (unplayed)
    mark so you can tell which media will not play (purpose paraphrased, not verbatim). The mark event
    for a CLEARED mark is indistinguishable from a played one on
    the wire (same JSON); brisvox must therefore know, locally, that it sent `clear` and treat any
    mark arriving after its own `clear` (until the next audio it sent after the clear) as "dropped"
    rather than "played". Order and timing of those returned marks relative to the `clear` are not
    specified (UNVERIFIED), nor whether a mark covering audio currently being played is returned at
    clear time or after the buffered sample finishes (UNVERIFIED).
  - Marks carry NO timestamp or played-sample count. A mark only says "everything sent before this
    mark has been played (or discarded)". Playback position at an arbitrary moment (needed by played-text truncation
    when the caller interrupts mid-chunk) is therefore not reported by Twilio: UNVERIFIED that no other
    field exists; none is documented. Granularity of the playback position = granularity of the marks.
  - Name: free string (page: "A custom value"). Maximum length and allowed characters not documented
    (UNVERIFIED; use short ASCII like a monotonically increasing counter or `"<epoch>:<seq>"`).
    Whether two pending marks may share a name: not stated (use unique names).
  - Brisvox design for played-text tracking (a proposal, not a doc claim): send a `mark` after every N ms of audio
    (e.g. every 100-200 ms or every TTS frame group) with name encoding (turn epoch, cumulative sample
    count sent). On a returned mark (not preceded by our own `clear`) advance the confirmed-played
    position; at barge-in the played position is
    `confirmed_marks + estimate of the unconfirmed tail` (the estimate comes from the local wall
    clock since the last mark; Twilio's own buffer depth is invisible), and the played text is truncated
    to that bound. Mark spacing trades accuracy against message count; every mark is one
    extra WS frame each way.
- Interruption (barge-in):
  - Send `{"event":"clear","streamSid":"..."}`. Per the page: empties the audio buffered on Twilio's
    side and returns the pending marks. Brisvox rules: (1) send `clear` the moment the turn epoch
    advances (engine rule "Clear-on-epoch-advance"), before any new-epoch audio; (2) stop sending old
    epoch `media`; (3) media sent after the `clear` plays normally (the page implies buffer reuse; not
    explicitly stated: UNVERIFIED), (4) discard returned marks per the previous section.
  - Race: audio already handed to the phone network before `clear` arrives still plays. The size of
    that residual (Twilio-side playout buffer, carrier jitter buffer) is not documented: UNVERIFIED;
    measure on a live call.
  - The changelog (2020-06-17) introduced Mark and Clear for building barge-in (the caller can interrupt
    playback; wording paraphrased); the Twilio samples `connect-basic` and `dialogflow-integration` are
    named there.
- Keepalive / timeouts:
  - No keepalive, ping interval, idle timeout, maximum call/stream duration or reconnect behaviour is
    documented on the pages fetched (UNVERIFIED). Each Media Stream uses one WebSocket connection
    (error 31921); if the connection drops the stream stops
    (31921) and there is no documented resume. Brisvox must treat a WS close as the end of the call leg.
  - Brisvox own timeouts apply (engine rule: every network await has an explicit timeout): e.g. time
    to first `start` after upgrade, and a stall timer on inbound `media` (Twilio sends inbound audio
    continuously while the call is up; the exact cadence is UNVERIFIED so size the stall timer
    generously and verify live).
  - Closing: closing the WebSocket from the server stops a bidirectional stream and lets the TwiML
    continue (31921 / stream TwiML page). Ending the call sends `stop`. Close code/reason Twilio uses
    on its side: not documented (UNVERIFIED).
- Request validation (X-Twilio-Signature on the upgrade request):
  - Media Streams page: "validate the `X-Twilio-Signature` header" (firewall page: how your
    application verifies that a Media Stream is coming from an authentic Twilio source). The Security
    page adds for voice WSS handshakes only: "try appending a trailing `/` character to the URL that
    you pass to the signature validation method" if verification of a WebSocket handshake request (for
    example Programmable Voice Media Streams) fails.
  - Algorithm (security page, HTTP requests): take the full request URL (scheme through query string);
    for POST append sorted (case-sensitive) parameter names+values with no delimiters (GET: params are
    in the query string); HMAC-SHA1 with the PRIMARY Auth Token as key; base64; compare to the
    `X-Twilio-Signature` header. A WS upgrade is a GET without params, so the signed string is just
    the URL (Twilio's own search-summary statement, not a verbatim doc sentence: UNVERIFIED). Because
    `<Stream url>` cannot carry a query string, the signed URL is the bare `wss://host/path` as written
    in the TwiML. Whether Twilio signs the URL with `wss://` or `https://` scheme, and whether the port
    `:443` is included, is NOT stated for WSS (UNVERIFIED): the security page says "Voice over HTTPS"
    drops the port (and username/password in the URL are always dropped). Implementation: try the
    candidate URL forms (as configured; with/without trailing `/`; `wss`/`https` scheme) and accept if
    any matches, constant-time compare; test live.
  - Primary Auth Token only (security page: if a secondary token was created, "you still need to use
    your previous AuthToken until the secondary one has been" promoted to primary).
    Behind a reverse proxy rebuild the public URL from configuration, not from the Host header you see.
  - Custom parameters (`start.customParameters`) arrive in a WS message AFTER the handshake and are
    not part of the signed URL; treat them as untrusted unless the handshake signature validated.
    (Twilio does not document signing them: UNVERIFIED that they are unsigned.)
  - Helper libs documented: Node `validateRequest`/`validateRequestWithBody`; Python/C#/Java/PHP
    `RequestValidator`; Ruby `Twilio::Security::RequestValidator`; Go `client.NewRequestValidator`
    (`Validate` / `ValidateBody`). No Rust SDK validator was found in these docs;
    a Twilio blog post "validate Twilio Event Streams webhooks in Rust" appeared in search results but
    concerns Event Streams, not this (not fetched: UNVERIFIED). Implement HMAC-SHA1 by hand
    (RustCrypto `hmac` + `sha1`, check crates.io for versions/licenses before adding; not researched here).
- Timestamps / interim results / tool-call deltas: not applicable. Relevant timing data: inbound
  `media.timestamp` (ms since stream start, string) and `chunk` counter (gaps = lost inbound audio,
  31930). `sequenceNumber` also counts every message (gaps detectable). Outbound `media` has no
  timestamp field.
- Concurrency / rate limits:
  - Bidirectional: only one Stream per Call. Unidirectional: up to four tracks per call, shared with
    SIPREC, Real-Time Transcription, Answering Machine Detection (the overview page names SIPREC and
    Real-Time Transcription as examples of forking features sharing the limit; Answering Machine
    Detection in that list is UNVERIFIED). Each stream = one WebSocket.
  - Per-account concurrent stream / call limits, messages-per-second limit, and pricing: NOT found on the
    pages fetched (UNVERIFIED). Voice pricing page: https://www.twilio.com/en-us/voice/pricing (not
    fetched). The overview page links to "Forked Audio Streams" for fork costs (not fetched).
  - Outbound buffer cap: 10 minutes (31931, above).
- Status callbacks (note only; not needed for the core adapter): `statusCallback` on `<Stream>` gets an HTTP
  request (method `statusCallbackMethod`, default POST) with `AccountSid`, `CallSid`, `StreamSid`,
  `StreamName` (defaults to StreamSid), `StreamEvent` (`stream-started` | `stream-stopped` |
  `stream-error`), `StreamError` (detail text), `Timestamp` (ISO 8601). Useful to debug failed
  starts (31920). Call-level status callbacks (call completed etc.) are separate Voice webhooks: not researched.
- Turkish support: not applicable to the transport (language-agnostic G.711 audio). Telephony quality
  note: 8 kHz mu-law narrowband is typically harder for STT; Deepgram etc. accept `mulaw` @ 8000 directly
  (see deepgram.md for the exact encoding parameters); Cartesia can output `pcm_mulaw` @ 8000
  directly (see cartesia.md). Whether Turkish `<Say>` etc. matters is irrelevant (brisvox does TTS).
- Mapping to the brisvox transport trait (crates/brisvox-transport, check the real trait when
  implementing) — proposal:
  - Inbound: base64-decode `media.payload` (only `track == "inbound"`) -> G.711 mu-law bytes -> decode
    via `brisvox_audio::codec` -> PCM16 8 kHz -> resample (rubato) to the pipeline rate. Drop messages
    whose `streamSid` differs from `start.streamSid`. Handle `dtmf` -> DTMF event. Handle `stop`
    -> end of session. Ignore unknown `event` values (forward compatibility).
  - Outbound: PCM16 -> resample to 8 kHz -> mu-law encode -> base64 -> `media`. Prefer 160-byte frames
    on the engine's 20 ms pacer (a design choice, see chunking above). Keep a reusable `String`/`Vec`
    buffer for base64 and the JSON frame (hot-path no-allocation rule).
  - Barge-in -> `clear`; marks -> playback position for played-text truncation (see Mark semantics).
- Gotchas found during implementation: see "Brisvox implementation" below.

## Brisvox implementation
Code: `brisvox_transport::twilio` (feature `twilio`, on by default; needs `websocket`). Tests:
`crates/brisvox-transport/tests/twilio.rs` (scripted Twilio client over real sockets, local mock of
the wire protocol), unit tests in `src/twilio/*`. Design notes: `docs/ARCHITECTURE.md`, "Twilio
transport".

### Setup
1. Run a `TwilioServer` (its own listener, the same server infrastructure as the generic WebSocket
   transport: call limit and 503, accept queue, pings, idle and write timeouts, shutdown), behind
   a TLS-terminating proxy that exposes it as `wss://agent.example.com/twilio/stream`.
   ```rust
   let mut server = TwilioServer::bind(
       ServerConfig { bind_addr: "127.0.0.1:8081".parse()?, ..ServerConfig::default() },
       TwilioConfig {
           path: "/twilio/stream".into(),
           auth_token: Some(Secret::new(std::env::var("TWILIO_AUTH_TOKEN")?)),
           public_url: Some("wss://agent.example.com/twilio/stream".into()),
           ..TwilioConfig::default()
       },
       clock, &cancel,
   ).await?;
   // then: loop { let link = server.accept(&cancel).await?; TransportSession::open(.., link, ..) }
   ```
2. Point the voice webhook of the Twilio number at an HTTP endpoint that answers with TwiML
   (brisvox does not serve it; any web framework or a Twilio Function will do). The recommended
   form has a **per-call stream URL**: the public URL plus a signed single-use segment minted from
   the call's `CallSid` ("Per-call stream URLs" below; a runnable webhook that does it, with tests,
   is `examples/phone-receptionist/twiml_server.py`, set `BRISVOX_TWILIO_STREAM_SECRET` on both
   sides). The TwiML has this shape:
   ```xml
   <?xml version="1.0" encoding="UTF-8"?>
   <Response>
     <Connect>
       <Stream url="wss://agent.example.com/twilio/stream/CAxxxx.1760000120.mac" name="brisvox">
         <Parameter name="agent" value="support-tr" />
       </Stream>
     </Connect>
   </Response>
   ```
   Legacy (static TwiML Bin, needs `BRISVOX_TWILIO_ALLOW_STATIC_URL=1` on the server, replayable
   signature): the same TwiML with `url="wss://agent.example.com/twilio/stream"`, the fixed
   public URL. Without a stream secret and without that flag the listener does not start.
   The URL starts with `TwilioConfig::public_url` and must not contain a query string (use
   `<Parameter>`). The values arrive in `CallInfo.metadata` as `custom.<name>`
   (`custom.agent = support-tr`). Anything after `<Connect>` runs only after brisvox closes the
   socket (engine hang-up), e.g. `<Say>` or `<Dial>` for a hand-off.

### What the adapter does
| Twilio message | Brisvox |
|---|---|
| `connected` | remembered (`twilio_protocol`, `twilio_version` metadata); optional |
| `start` | `CallInfo` (below); media format must be `audio/x-mulaw`, 8000, 1 channel when stated, else the connection is closed with 1008 |
| `media` (`track` `inbound` or absent) | base64 -> mu-law -> PCM16 8 kHz `AudioFrame` (`WireEvent::Audio`, DropOldest); other tracks and other `streamSid`s are ignored |
| `dtmf` | `WireEvent::Dtmf` (single key `0-9 * # A-D`; else a fault) |
| `mark` | `e<n>`: `MarkAck` with the engine's id (`TransportEvent::MarkPlayed`); `p<n>`: progress bookkeeping; anything else or not in flight: dropped (`marks_unmatched`) |
| `stop` or close frame | hang-up (`StopReason::PeerHangup`); a vanished socket is `PeerDisconnected` |
| unknown `event` | ignored |
| malformed JSON, bad base64, missing fields | non-fatal fault (`TransportEvent::Error`); more than 16 per call close it (1008) |

| Engine | Twilio |
|---|---|
| `WireOut::Audio` (PCM16 8 kHz) | `{"event":"media","streamSid":..,"media":{"payload":..}}` with 160 mu-law bytes (20 ms, `chunk_bytes`) per message; larger engine frames are cut into that size |
| `Mark{id}` | `{"event":"mark","streamSid":..,"mark":{"name":"e<n>"}}`; the id stays in the adapter |
| (adapter) | a progress mark `p<n>` after every `mark_interval` (200 ms) of audio |
| `Clear{epoch}` | `{"event":"clear","streamSid":..}` after dropping the marks in flight |
| `Hangup` | close frame 1000 (Twilio accepts no `stop` from the server); immediate, see "Ending the call" |

`CallInfo.metadata`: `transport=twilio`, `sample_rate=8000`, `encoding=mulaw`, `stream_sid`,
`call_sid`, `account_sid`, `tracks`, `twilio_protocol`, `twilio_version`, `peer_addr`,
`custom.<name>`; `call_id` is the `callSid`. All outbound messages use the `streamSid` of `start`.

Audio rates: the adapter converts mu-law <-> PCM16 but does not resample (as the generic
adapter): the engine must produce 8 kHz audio for a Twilio call (the output chain resamples). Bot audio at
another rate is dropped and reported once per call as a fault. No audio is passed through as
mu-law: the core is PCM16 only.

### Marks, clear and played-text tracking
- Twilio returns a `mark` both when audio played and when a `clear` flushed it, with identical JSON.
  The adapter tells them apart locally, as the notes under "Mark semantics" require: it keeps the
  marks in flight (name -> engine id, at most 512) and forgets all of them when it sends `clear`.
  An echo that arrives afterwards finds nothing and is dropped. This is more precise than "marks
  after a clear are dropped until new audio": a mark sent after the clear has a new name and
  counts. The core reports the engine's unechoed marks as `MarkCleared` at the same moment.
- The wire name is a letter and a counter (`e0`, `p17`), never the engine's id: Twilio documents
  no length or character limit for names (UNVERIFIED), so ids of any length and content work.
- Progress marks give `TwilioStats::max_echo_latency` (time from sending a mark to its echo,
  which is the playout delay plus the audio buffered ahead of it: open item 8) and
  `max_unplayed` (the most audio sent but not yet known played).
- Playback tracking for played-text truncation has two signals. The engine's utterance-end marks come back as
  `TransportEvent::MarkPlayed` (the utterance was heard whole). Every echoed mark, progress or
  engine, also yields the audio sent after it (not yet played) and the adapter reports that as
  `WireEvent::Lead`, forwarded as `TransportEvent::PlaybackLead { lead }` (a level; a new report
  replaces a queued one). For a barge-in in the middle of an utterance the heard position is
  `PlaybackPosition.played - PlaybackLead.lead`, never below zero; it replaces the fixed
  `lead_margin` guess with what Twilio's buffer actually holds, to within the 200 ms mark spacing
  (Twilio sends no position inside a mark interval). **Status:** the transport side is
  done and tested (a lead report per echoed mark, coalesced to the latest, reset by a clear),
  `brisvox-engine` stores it (`PlayoutBook::set_client_lead`) and `PlayoutTruncation` reads it:
  `heard = min(released, total_audio) - lead`, where a reported lead REPLACES the fixed
  `lead_margin` (in both directions: a lead above the margin cuts more, a lead below it cuts
  less), less the audio of later utterances in the client's buffer; the lead in force when a
  `Clear` covered the utterance is the one used, and an uninterrupted response loses no lead.
  The replace rule is reasoned, not measured. **Open
  risk (RISKS.md R15):** a small reported lead (about 50 ms) drops the margin for audio still
  queued on the engine-to-Twilio path, which the lead (audio sent after the last echoed mark, as
  Twilio's buffer holds it) may not cover, so one more word than was heard can be kept. The
  conservative alternative is `max(lead, lead_margin)`; switch to it if a live Twilio measurement
  (released position against the echo times of progress marks, with the wire queue loaded) shows
  the engine-side queue is not in the reported lead. `clear_hook` must be installed for the lead
  to be tied to its clear.

### Ending the call
Closing the socket is the only hang-up, and it is immediate: the adapter does not wait for audio
still queued toward Twilio or buffered by Twilio, and the docs do not say whether Twilio plays
what it already buffered once the socket is closed (UNVERIFIED, open item 9). A goodbye utterance
followed directly by `TransportCommand::Stop` can therefore be cut off. The engine must send the
audio, then an utterance-end `TransportCommand::Mark`, wait for its `TransportEvent::MarkPlayed`
(with its own timeout, since a mark can also be lost), and only then send `Stop`.

### Request validation
With an `auth_token`, the upgrade needs `X-Twilio-Signature` = base64(HMAC-SHA1(token, URL)) or the
answer is HTTP 403 (no header, bad base64, wrong token, wrong URL). The HMAC comes from `ring`
(already in the tree through rustls; `HMAC_SHA1_FOR_LEGACY_USE_ONLY` is the name of the primitive,
the algorithm is the one Twilio mandates) with a constant-time compare. Because the signed URL form
for `wss` is not documented (UNVERIFIED, open item 1), the signature may match any of: the
configured `public_url`; the same with `https` for `wss` (and `http` for `ws`); with the default
port `:443`/`:80` added or removed; with a trailing `/` added or removed (up to 8 forms, all
public information). Unit tests pin HMAC-SHA1 to RFC 2202 and the digest to Twilio's published
worked example. Requests for any other path get 404 before the signature is looked at. Without an
auth token nothing is checked: use that only behind another check. Auth token and `public_url` are
validated together at startup (`TwilioConfig::validate`); the token is a `Secret` and never
printed. The signature is computed over `public_url` as configured, so `path` (the route the
listener serves) may differ from the path of `public_url`: a gateway can serve
`wss://agent.example.com/voice/twilio` and forward to `/twilio/stream`. The proxy must pass the
`X-Twilio-Signature` header through unchanged.

What the signature does not give: it authenticates the URL, and for a `<Stream url>` without a
query string the URL is the same on every call, so the signature is one constant value. Anyone who
has seen it once (a proxy or access log) can open upgrades with it until `max_calls` is reached.
`start.customParameters` are not signed at all. That is why a signed listener now needs per-call
URLs (`stream_secret`, see "Per-call stream URLs" below) or the explicit legacy flag
`allow_static_url`; the fixed-URL mode described here is the legacy one. Treat custom parameters
as untrusted either way.

### Per-call stream URLs (review MEDIUM-1)

The signature above covers the URL only, and the URL is the same for every call, so a signed
upgrade that was captured once (a proxy or access log) is valid for ever. Brisvox therefore makes
the URL different for every call and accepts each URL once.

Sources, read 2026-10-13: https://www.twilio.com/docs/usage/security (the signed string is the full
request URL including the query string; for a POST the sorted parameters follow, a WebSocket
upgrade is a GET without any; HMAC-SHA1 with the primary Auth Token, base64; "Voice over HTTPS"
callbacks drop the port; a trailing slash may be missing; for voice WSS handshakes it says to try a
trailing `/` if validation fails), and https://www.twilio.com/docs/voice/twiml/stream (the `url`
attribute "does not support query string parameters", so per-call data goes either into a custom
`<Parameter>`, which arrives after the handshake and is not signed, or into the URL **path**, which
is signed). Twilio signs the path like any other part of the URL, so a per-call path segment is
covered by `X-Twilio-Signature` with no change to the check. The URL forms tried
(`signature::candidate_urls`: scheme `wss`/`https`, with or without `:443`, with or without a
trailing `/`) apply to the full per-call URL unchanged.

The segment is `<callSid>.<exp>.<mac>` appended to the base URL (`BRISVOX_TWILIO_PUBLIC_URL`):

```
mac = base64url_nopad( HMAC-SHA256( secret, "brisvox-twilio-stream-v1|" + callSid + "|" + exp ) )
```

`callSid` is the call's SID (ASCII letters and digits), `exp` the Unix time in seconds after which
the URL is dead (at most one hour ahead), `secret` is `BRISVOX_TWILIO_STREAM_SECRET` (at least
32 bytes). The upgrade is accepted only if all hold: the segment parses and its MAC verifies
(constant time); `exp` is in the future on the injected clock; the segment was not used before
(a bounded set, entries dropped at their expiry, at most 65 536 live ones, a full set refuses);
`X-Twilio-Signature` matches the full per-call URL; and the `start` message that follows carries
the same `callSid` (a URL minted for one call cannot open another: close code 1008). The segment
is used up only after the signature held, so someone who holds a URL but no signature cannot burn
a legitimate call's token. **The used-set is per process**: a restart empties it and several
instances behind a load balancer each have their own, so a captured URL with a valid signature is
replayable against another instance (or after a restart) until its `exp`, at most one hour ahead
(120 s for a URL the server mints itself). Keep `exp` short (the lifetime of the call set-up, not
the call) and pin the stream path to one instance if replay matters. Segments the server mints
for the same call within one second get consecutive `exp` values, so each is a distinct token.
Everything else is HTTP 403 (404 for a path that is not the base or the
base plus one segment).

Who mints it:

- **Streams the server restarts itself** (DTMF through call control, a failed warm transfer): the
  server writes the TwiML and signs the URL (`StreamUrlSigner`, lifetime 120 s).
- **The first stream of an inbound call** is named by the operator's webhook, which mints the
  segment with the same secret:

```python
import base64, hashlib, hmac, time

def stream_url(base, secret, call_sid, ttl=120):
    exp = int(time.time()) + ttl
    msg = f"brisvox-twilio-stream-v1|{call_sid}|{exp}".encode()
    mac = hmac.new(secret.encode(), msg, hashlib.sha256).digest()
    mac = base64.urlsafe_b64encode(mac).rstrip(b"=").decode()
    return f"{base.rstrip('/')}/{call_sid}.{exp}.{mac}"
```

```js
const crypto = require("crypto");
function streamUrl(base, secret, callSid, ttl = 120) {
  const exp = Math.floor(Date.now() / 1000) + ttl;
  const mac = crypto.createHmac("sha256", secret)
    .update(`brisvox-twilio-stream-v1|${callSid}|${exp}`).digest("base64url");
  return `${base.replace(/\/+$/, "")}/${callSid}.${exp}.${mac}`;
}
```

  Known answer (secret `0123456789abcdef0123456789abcdef-secret`, call `CAabc123`, exp
  `1900000000`): `CAabc123.1900000000.ixBbTZftQyn5JYCeFPFULx9O_UM1uxv0OZ_JhN6GIm0` (a unit test
  pins it).
- **Legacy**: a deployment whose own TwiML cannot do that sets `BRISVOX_TWILIO_ALLOW_STATIC_URL=1`
  (`TwilioConfig::allow_static_url`): the fixed URL is accepted again (also next to per-call URLs
  when a secret is set), with the replay risk described above, and the server logs a warning at
  startup. Without a secret and without the flag the Twilio listener does not start.

**Migrating from the fixed URL.** (1) Set `BRISVOX_TWILIO_STREAM_SECRET` (32 bytes or more) and keep
`BRISVOX_TWILIO_ALLOW_STATIC_URL=1` for the transition, so both forms are accepted. (2) Change the
webhook that answers inbound calls to return `<Stream url="wss://host/twilio/stream/<segment>">`
with a segment minted per call (the snippets above). Outbound calls and server-restarted streams
need nothing. (3) Check that calls connect (a refused segment is an HTTP 403 to Twilio). (4) Remove `BRISVOX_TWILIO_ALLOW_STATIC_URL`; the fixed URL is then refused.

The listener's `path` is the base: `/twilio/stream` accepts `/twilio/stream/<segment>`. A gateway
that rewrites the public path must keep the suffix (`/voice/twilio/<segment>` ->
`/twilio/stream/<segment>`). Not verified live: that Twilio signs the per-call URL in the form the
candidates cover (the same open item 1 as for the fixed URL).

### Defensive handling of UNVERIFIED points
- Numbers as strings or numbers (`sequenceNumber`, `chunk`, `timestamp`, `sampleRate`, `channels`):
  both parse. The three the adapter does not depend on (`sequenceNumber`, `chunk`, `timestamp`) and
  `connected.protocol` / `version` are read softly: any other type (`"1.0"`, `""`, a float, an
  object) reads as absent and never costs the message that carries it. A `connected` message that
  cannot be read is skipped. `sampleRate` and `channels` are checked, so a malformed one is a
  protocol error on `start`. `customParameters` values that are not strings are kept as JSON text.
- Binary frames from Twilio: ignored. Unknown events and fields: ignored. A missing `connected`:
  accepted. A missing `track` on `media`: treated as inbound.
- Keepalive: Twilio is not documented to ping or answer pings. The server pings every
  `ping_interval` and closes after `ping_timeout` without any frame; inbound `media` (every 20 ms
  while the call is up, UNVERIFIED) counts as life, so set `ping_timeout` well above a plausible
  silent gap (default 30 s).
- Chunk size: 160 bytes per outbound message is a brisvox choice, not a documented requirement;
  `TwilioConfig::chunk_bytes` changes it.
- Inbound audio loss: skipped `media.chunk` numbers are counted (`inbound_chunks_lost`), not
  reported to the engine.
- A `clear` while Twilio has nothing buffered is still sent (cheap, and the engine's ordering rule
  is "clear before new-epoch audio").

### Gotchas found while implementing
- `tokio-tungstenite`'s `Callback` for the upgrade returns the full HTTP response as the error
  (clippy `result_large_err`); the adapter implements `Callback` on a small struct and boxes the
  refusal internally.
- The accept loop of the generic server is shared by passing the per-connection serve function
  (`ServeFn`); `TwilioServer` wraps a `WebSocketServer`, so accept, shutdown and the active-call
  count are the same code.
- A `clear` must forget the marks in flight *before* the message goes out; otherwise an echo that
  races the clear would be taken for a played mark.
- Twilio accepts nothing but `media`/`mark`/`clear` from the server, so there is no way to send an
  error or a goodbye: faults go to the engine, fatal ones close the socket with a reason.

## Open items to settle with a live test (needs a Twilio account, a number and a public wss URL)
1. Exact URL form Twilio signs for the WSS upgrade (scheme, port, trailing slash) and that header name casing is `X-Twilio-Signature`.
2. Inbound `media` cadence and frame size (ms per message); behaviour under backpressure (31930).
3. Residual audio heard after `clear` (ms), order/timing of the marks returned by `clear`, and whether a mark for the audio currently playing is returned immediately.
4. Whether media sent right after `clear` plays immediately and whether mark names may repeat or have a length limit.
5. Idle/keepalive behaviour (does Twilio ping? does it close idle sockets?), close codes, and what happens to the call when brisvox closes the WS (TwiML continues).
6. Whether numeric fields are ever sent as JSON numbers; whether binary frames ever occur.
7. Whether `<Parameter>` inside `<Connect><Stream>` works exactly as in `<Start><Stream>` (expected yes).
8. Playback latency Twilio adds between our `media` and the caller's ear (feeds the LATENCY.md budget).
9. Whether Twilio plays audio it buffered after brisvox closed the WebSocket (decides whether the "wait for the final `MarkPlayed` before `Stop`" rule is needed or merely safe), and whether a bounded drain in the adapter is worth adding.
10. Whether `PlaybackPosition - PlaybackLead` tracks what the caller heard (compare with the barge-in point reported by a test caller) and how stale `PlaybackLead` is at the 200 ms mark spacing.

## Sources (all fetched 2026-10-08)
- https://www.twilio.com/docs/voice/media-streams (and .md)
- https://www.twilio.com/docs/voice/media-streams/websocket-messages (and .md; JSON examples taken from the .md)
- https://www.twilio.com/docs/voice/twiml/stream (and .md; TwiML examples, attributes, `<Parameter>`, status callback fields)
- https://www.twilio.com/docs/voice/twiml/connect (regions note; `<Connect>` action behaviour; no `<Stream>` section)
- https://www.twilio.com/docs/usage/security (and .md; signature algorithm, WSS trailing-slash note, `bodySHA256`)
- https://static1.twilio.com/docs/global-infrastructure/firewall-configurations/media-streams-configuration (port 443, any public IP, validate signature)
- https://www.twilio.com/docs/api/errors/31920 (WebSocket handshake error), /31921 (close error), /31923 (malformed URL), /31924 (protocol error), /31930 (buffer overflow, inbound), /31931 (media discarded, outbound 10 min), /31951 (invalid message)
- https://www.twilio.com/en-us/changelog/bi-directional-streaming-support-with-media-streams (2020-06-17; introduces Mark and Clear)
- Web search results (summaries, not primary text, used only where labeled UNVERIFIED): chunk size not documented; custom parameters not in the signed URL.
- Not fetched: Twilio sample apps (`twilio/media-streams` node/connect-basic, dialogflow-integration), the Stream REST resource page (https://www.twilio.com/docs/voice/api/stream-resource), voice pricing.

Fact-checked 2026-10-08: 58 claims checked, 11 corrected. Corrections: (1) outbound payload format quote
replaced with the real wording ("encoded `audio/x-mulaw` with a sample rate of `8000`..."; the unfound
sentence "Providing header bytes causes the media to be streamed incorrectly" and "raw mulaw/8000" removed);
(2) "Audio can be any size" -> "The audio can be of any size."; (3) mark return wording is "or if there is no
audio buffered" (no word "immediately"; timing now UNVERIFIED); (4) clear quote replaced with the real wording
and the invented "so you know which media was discarded" quote dropped; (5) mark name description is "A custom
value", not "a name of your choosing"; (6) secondary Auth Token quote corrected; (7) `stop` semantics: messages
page says only ending the call stops a bidirectional stream, overview page and 31921 say closing the WebSocket
does too (both now stated; the old 31921 and "Bidirectional Streams stop only..." quotes removed as non-verbatim);
(8) "Only wss is supported"/"Query string parameters are not supported" and other non-verbatim quotes (changelog,
31930, 31931, 31951, "one WebSocket connection") turned into paraphrases; (9) Ruby/Go validator names completed;
(10) a media example on the messages page shows `track: "outbound"`, noted; (11) Answering Machine Detection in
the shared four-track list marked UNVERIFIED (overview names only SIPREC and Real-Time Transcription).
