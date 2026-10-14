# Telephony call control (DTMF, transfer, voicemail/AMD) — Twilio and Telnyx

Scope: DTMF in/out, warm/cold transfer (tool-callable), voicemail / answering-machine
detection strategy and leave-message flow. Related: `docs/providers/twilio.md` (Media Streams wire protocol, request
validation), `docs/providers/telnyx.md` (Telnyx media streaming wire protocol), `docs/providers/g711.md` (codec).
Researched: 2026-10-08. Every fact below was read from the pages listed under Sources on that date. Anything not
confirmed on a fetched page is marked UNVERIFIED. Statements labelled "Proposal" or "Brisvox design" are engineering
choices, not documentation claims. This is call control (REST/webhooks next to the media WebSocket), not an
STT/LLM/TTS provider.

Scope boundary (from `twilio.md`): brisvox core is the WebSocket SERVER of the media stream and does not serve TwiML or
webhooks. Everything on this page that is REST (update call, participants, `send_dtmf`, `transfer`) is an OUTBOUND
HTTPS call that a brisvox tool/adapter makes (reqwest 0.13.5, already in the workspace `Cargo.toml`); everything that is
a callback (status callbacks, AMD results, conference events, Telnyx webhooks) is served by the user's HTTP app or by an
optional brisvox webhook module (not decided here).

## 0. Capability matrix (what the docs say works)

| Capability | Twilio (Media Streams, `<Connect><Stream>`) | Telnyx (Call Control + media streaming) |
|---|---|---|
| DTMF from caller to bot | Yes: `dtmf` WebSocket event, bidirectional streams only, inbound direction only | Yes: `dtmf` WebSocket frame (`dtmf.digit`, `occurred_at`); webhook `call.dtmf.received` is documented as a Gather result |
| DTMF from bot to far end, over the stream | NO (documented: not supported outbound) | NO (only `media`/`mark`/`clear` are accepted; no DTMF frame documented) |
| DTMF from bot to far end, other route | REST update call with TwiML `<Play digits="...">` (replaces the TwiML, see 2.2) | REST `POST .../actions/send_dtmf` (no webhooks; interplay with an active stream not documented) |
| Cold transfer | REST update call with `<Dial>` TwiML (or `<Connect action>` fallthrough); `<Refer>` only for SIP legs | `POST .../actions/transfer` (documented description: original leg stays active if the transfer fails) |
| Warm transfer | Conference + Participants API, or `<Dial><Number url=...>` whisper screening | Conference commands, or Dial with `link_to` + Bridge; `refer` for SIP REFER |
| AMD | Calls API / Participants API / `<Dial><Number>` / `<Dial><Sip>`; `AnsweredBy` incl. `machine_end_beep` | `answering_machine_detection` on Dial/Transfer: `detect`, `detect_beep`, `detect_words`, `greeting_end`, `premium` |
| AMD price | $0.0075 per call | $0.002 per call (standard), $0.0065 per call (premium) |
| Idempotency key on commands | none documented (UNVERIFIED) | `command_id` on every command |

## 1. Twilio

### 1.1 REST basics (auth, encoding)
- Docs: https://www.twilio.com/docs/usage/requests-to-twilio (fetched 2026-10-08).
- Auth: HTTP Basic. "use the Account SID as the HTTP Basic authentication username and the Auth Token as the password"
  (page wording). Alternative: API key SID as username and API key secret as password; the page says to prefer API keys for
  applications unless a reference says to use Account SID + Auth Token. Docs' own curl examples use `-u $TWILIO_API_KEY:$TWILIO_API_SECRET`.
  Brisvox: support both; never log the token/secret (same rule as `twilio.md`).
- Base URL: `https://api.twilio.com/2010-04-01/` (all examples; the page does not state it as a standalone rule); resource URLs end in `.json`.
- POST bodies: `application/x-www-form-urlencoded` or `multipart/form-data` ("Twilio APIs expect ..."). Repeated
  parameters (e.g. several `StatusCallbackEvent`): the Call create page says "Send multiple values as separate parameters" (page wording: "use a separate parameter for each").
- Rate limits / concurrency limits / error body format: NOT covered on the fetched pages (UNVERIFIED). Error bodies in
  samples have `error_code` and `error_message` fields only.
- Idempotency: no idempotency header or key is documented for Calls or Participants (UNVERIFIED). Brisvox must provide
  at-most-once itself (engine rule "SideEffect tools never run twice"); after a timeout, GET the call before any retry.

### 1.2 Call resource: create, update, fetch
Docs: https://www.twilio.com/docs/voice/api/call-resource (and `.md`), https://www.twilio.com/docs/voice/tutorials/how-to-modify-calls-in-progress (fetched 2026-10-08).

- Create: `POST https://api.twilio.com/2010-04-01/Accounts/{AccountSid}/Calls.json`. Required `To`, `From`. Exactly the
  documented example:
  ```bash
  curl -X POST "https://api.twilio.com/2010-04-01/Accounts/$TWILIO_ACCOUNT_SID/Calls.json" \
  --data-urlencode "Url=http://demo.twilio.com/docs/voice.xml" \
  --data-urlencode "To=+15558675310" \
  --data-urlencode "From=+15017122661" \
  -u $TWILIO_API_KEY:$TWILIO_API_SECRET
  ```
  Create parameters relevant here (page table): `Url` (absolute URL returning TwiML), `Twiml` ("Max 4000 characters."; page wording: "If both `twiml` and `url` are provided then `twiml` parameter will be ignored", i.e. `Url` wins on Create),
  `ApplicationSid`, `Method` (GET|POST, default POST), `FallbackUrl`/`FallbackMethod`,
  `StatusCallback`/`StatusCallbackMethod` (default POST; "Without events, only `completed` is sent"),
  `StatusCallbackEvent` (`initiated`, `ringing`, `answered`, `completed`; default `completed`), `Timeout` (default 60, max 600; a
  5-second buffer may be added), `TimeLimit`, `SendDigits` (up to 32 characters; page wording: "If both `SendDigits` and `MachineDetection` parameters are
  provided, then `MachineDetection` will be ignored"), `Record` etc., and the AMD set in 1.7. Dial-out result fields: `sid` (`CA...`), `status`, `direction`, `answered_by`, `parent_call_sid`, ...
- Update (redirect / hang up a LIVE call): `POST https://api.twilio.com/2010-04-01/Accounts/{AccountSid}/Calls/{Sid}.json`
  (`Sid` = 34 chars, starts with `CA`). Page description (paraphrase; the exact earlier-quoted sentence "Modifies an active call. You can redirect an in-progress call
  to new TwiML or terminate it." was NOT found verbatim, the page says "Updating a Call allows you to modify an active call" and that a POST to a Call instance lets you redirect an in-progress call or terminate it). Parameters (page table):
  | Param | Meaning |
  |---|---|
  | `Url` | Absolute URL returning TwiML; called with `Method` when the call connects |
  | `Method` | GET or POST, default POST (ignored if `application_sid` present) |
  | `Twiml` | TwiML used without fetching `Url`; mutually exclusive with `Url` (page wording: "Twiml and url parameters are mutually exclusive") |
  | `Status` | `canceled` or `completed` |
  | `FallbackUrl`, `FallbackMethod` | used on error fetching/executing the TwiML at `Url` |
  | `StatusCallback`, `StatusCallbackMethod` | status updates; to change `StatusCallback` of a call "it is required to set the Url in the same statement" |
  | `TimeLimit` | max call duration in seconds |
  The maximum length of `Twiml` on UPDATE is not stated (only the Create page says 4000 characters): UNVERIFIED, keep
  update TwiML under 4000 characters or use `Url`.
  Example of the documented shape (inline TwiML update; the docs show this as SDK code, the curl form below is derived
  from the Create example and the parameter table, not a verbatim sample):
  ```bash
  curl -X POST "https://api.twilio.com/2010-04-01/Accounts/$TWILIO_ACCOUNT_SID/Calls/$CALL_SID.json" \
  --data-urlencode "Twiml=<Response><Dial>+905551112233</Dial></Response>" \
  -u $TWILIO_ACCOUNT_SID:$TWILIO_AUTH_TOKEN
  ```
  Response: the updated Call resource (fields: `account_sid, answered_by, api_version, caller_name, date_created,
  date_updated, direction, duration, end_time, forwarded_from, from, from_formatted, group_sid, parent_call_sid,
  phone_number_sid, price, price_unit, queue_time, sid, start_time, status, subresource_uris, to, to_formatted, trunk_sid,
  uri`). `status` values: `queued, ringing, in-progress, completed, busy, failed, no-answer, canceled`. Error codes for
  updating a completed/unknown call: not listed on the page (UNVERIFIED). The adapter therefore does not rely on one status code when it ends a warm transfer's agent leg: after a 4xx other than 404 it reads the leg and treats a final status as "already over", and ends a queued or ringing leg with `Status=canceled` (the update table lists `canceled` or `completed`).
- Semantics (tutorial page): the request "instructs Twilio to make an immediate change [to] the TwiML document that Twilio is
  using in a phone call". Not stated: what happens to the verb currently running (the page says nothing). Hang up:
  `Status=completed` ("To end a phone call by your choice, pass a `completed` status to a CallSid in progress"). The page also says Twilio ends a call
  on `<Hangup>` or when it runs out of TwiML documents to process.
- Parent/child (call-resource page, paraphrase of the "modifying active calls" notes): redirecting an active call to another phone number makes Twilio create "an entirely new Call instance for that new phone number"; the original call is the parent call, and each additional dialed number is a child call, each with its own Call SID.
  A parent call executing a `<Dial>` "is considered in-progress by Twilio" even after redirection, so "you must use `Status=completed` to end it."
- Fetch: `GET https://api.twilio.com/2010-04-01/Accounts/{AccountSid}/Calls/{Sid}.json`.

### 1.3 What a REST update does to a bidirectional Media Stream
This is the central unknown for DTMF and transfer on Twilio.
- Documented: `<Connect><Stream>` creates the bidirectional stream; "Twilio doesn't execute subsequent TwiML instructions" and
  "Twilio executes the remaining TwiML instructions only after your server closes the WebSocket connection" (stream TwiML page).
  The overview page: "You can stop a bidirectional Media Stream by closing the WebSocket connection from your server or by
  ending the Call." The messages page says "the only way to stop a Stream is to end the call" (the contradiction is recorded in `twilio.md`).
  The stream TwiML page says "You can also update the call with TwiML instructions using the Update a Call resource" (page wording); it does not say what happens to the Stream in that case (our reading; no such sentence is quoted from the page).
  The general `<Connect>` page: "If there is no further verb, Twilio will end the phone call", and "If you do not provide an `action` URL,
  `<Connect>` will finish and Twilio will move on to the next TwiML verb in the document". `<Connect action>` text: "When the `<Connect>` verb ends,
  Twilio sends an HTTP request to this URL"; it does not list Stream-specific parameters.
- NOT documented (UNVERIFIED, live-test item 1): whether a REST update with new TwiML closes the stream, sends a `stop` frame,
  closes the socket from Twilio's side, or in what order; whether a new `<Connect><Stream>` inside the update TwiML is allowed
  while the old stream is still ending (media-streams overview page: "For bidirectional Streams, you can have only one Stream per Call"; the `<Stream>` TwiML page only says a Stream `name` must be unique per Call).
- Safe sequencing rules derived from the documented facts (Brisvox design):
  1. Never close the WebSocket first when a hand-off TwiML must follow, unless the original TwiML has a `<Connect action>`/next verb
     that does the hand-off: with no further verb the call ends.
  2. So: finish playing the farewell/hand-off audio (utterance-end mark played), call REST update, wait for HTTP 200, then keep
     the socket open and treat the next `stop`/close as expected (not a user hang-up) for a bounded time (e.g. 5 s), then close.
  3. Pre-planned documented alternative: original TwiML `<Connect action="https://app/after-stream"><Stream .../></Connect>`; brisvox
     closes the socket, Twilio requests the `action` URL (UNVERIFIED that closing a Stream triggers `action`), the user's app returns
     `<Dial>` TwiML chosen from state brisvox has stored (needs shared state keyed by `CallSid`). Rejected as the default: requires
     the user's HTTP app and extra state; REST update is self-contained.

### 1.4 DTMF
Inbound (caller presses keys): see `twilio.md` ("Protocol"): `{"event":"dtmf","streamSid":..,"sequenceNumber":"5","dtmf":{"track":"inbound_track","digit":"1"}}`,
bidirectional streams only. Overview page (fetched 2026-10-08): "DTMF isn't supported with unidirectional streams." "DTMF is
supported with bidirectional Media Streams only in the inbound direction, from Twilio toward your media server." Digit alphabet
beyond one key and whether the tone audio is also present in the `media` payload: UNVERIFIED (the messages page does not say). No duration or timestamp is sent with a `dtmf` event.

Outbound (bot presses keys on the far end, e.g. IVR navigation):
- Over the stream: NOT supported (same overview page: "Sending DTMF outbound from your media server toward Twilio is not supported"; the server may only send `media`, `mark`, `clear`).
- TwiML `<Play digits="...">` (https://www.twilio.com/docs/voice/twiml/play, fetched 2026-10-08):
  - `digits` allowed characters: `0-9`, `A`, `B`, `C`, `D`, `W`, `w`, `#`, `*`. Lowercase `w` = 0.5 s pause, uppercase `W` = 1 s pause
    (page prose: `www3` = 1.5 s before tone 3; the page's own samples are inconsistent between `www3` and `wwww3`). No maximum length or tone duration is stated.
  - The page describes `<Play digits>` as playing DTMF tones "during a call" and suggests testing an IVR menu. It does NOT say that the
    tones go to a particular leg; for dialing a number with an extension the page says use `<Number sendDigits>`. `<Play digits>` cannot be used inside `<Gather>` (a nested `<Play>` with a file is allowed, digits are not).
  - Example (verbatim): `<Response><Play digits="www3"></Play></Response>`.
- Other digit-sending parameters: Create Call `SendDigits` (up to 32 chars) and `<Dial><Number sendDigits="wwww1928">` (page 'number': plays
  tones "once the call is answered"; same alphabet `0-9 A B C D W w # *`). These apply to calls Twilio places, not to an already-running stream.
- Consequence for a call that is in a Media Stream: the only documented way to emit DTMF mid-call is to REPLACE the TwiML via Update Call
  (1.2). The update ends the `<Connect><Stream>` flow in some way (1.3) so after the digits the TwiML must start a NEW stream:
  ```xml
  <Response>
    <Play digits="ww1234#"/>
    <Connect>
      <Stream url="wss://agent.example.com/twilio/stream" name="brisvox">
        <Parameter name="resume" value="SESSION_ID"/>
      </Stream>
    </Connect>
  </Response>
  ```
  Consequences (design, UNVERIFIED in practice): a new WebSocket upgrade, a new `streamSid`, a gap in both audio directions, the
  new `start.customParameters.resume` must be mapped back to the live session by an adapter (`CallInfo.metadata["custom.resume"]`), and the
  Twilio signature check applies to the new upgrade as usual. This is heavy; do it only for deliberate IVR navigation.
- In-band alternative (Proposal, reliability UNVERIFIED): synthesize the two-tone DTMF waveform as PCM16 8 kHz in the engine, mu-law encode, and
  send it as normal `media` frames with the audio pacer. No Twilio doc says in-band tones sent on the stream are passed to the far end as DTMF
  or detected by an IVR; the far-end behaviour of G.711 tones is carrier- and IVR-dependent. Parameters to expose as config, tested live:
  tone ms, gap ms, level. For orientation only: Telnyx's own `send_dtmf` documents `duration_millis` 100-500, default 250 (section 2.2). Frequency
  table is the ITU keypad standard; this research confirmed only keys 5 (770+1336 Hz) and 8 (852+1336 Hz) via the dtmf_tones repo README, the rest of the
  table (rows 697/770/852/941, columns 1209/1336/1477/1633) is UNVERIFIED by a fetched source: cross-check with a decoder in a unit test (Goertzel).
- Event wiring (brisvox): `WireEvent::Dtmf` already exists (see `twilio.md` table, "dtmf -> WireEvent::Dtmf"); brisvox delivers it as `Data::Dtmf`
  to the turn state machine/flows and exposes it to tools (e.g. a `dtmf` message in the LLM context: "[caller pressed 5]"). Outbound: a new transport
  command `SendDtmf { digits }` per transport: Telnyx -> `send_dtmf`; Twilio -> either the REST-update route above or in-band synthesis, selected by config; others -> "unsupported" error.

### 1.5 Transfer on Twilio
Primitives (all fetched 2026-10-08): TwiML `<Dial>`, `<Number>`, `<Conference>`, `<Refer>`, `<Redirect>`; REST Calls update; Conference and Participant resources.

#### 1.5.1 Cold transfer (bot says goodbye, caller is connected to a number)
- REST: Update Call (1.2) with
  `Twiml=<Response><Dial callerId="+90..." answerOnBridge="true" timeout="30" action="https://app/dial-result">+90555...</Dial></Response>`
  (attributes from the `<Dial>` page: `action`, `answerOnBridge` (default false), `callerId`, `timeout` default 30 (5-600, +5 s buffer), `timeLimit` default 14400,
  `hangupOnStar`, `record`). The caller's call becomes the parent; `<Dial>` creates a child call with its own `CallSid`; both legs bill (billing statement not
  fetched: UNVERIFIED). A plain-text number is the same as nested `<Number>`; E.164 recommended.
- `callerId` rules (`<Dial>` page): the dialed party sees the inbound caller's number by default; with `<Number>` `callerId` may be the `To` or `From` of Twilio's request, a
  purchased Twilio number or a verified number; for inbound SIP calls it cannot be the `To`/`From`.
- Outcome reporting: with `action`, Twilio sends `DialCallStatus` (`completed`, `answered` (conference), `busy`, `no-answer`, `failed`, `canceled`), `DialCallSid`, `DialCallDuration`,
  `DialBridged`. Without `action`, Twilio moves to the next verb; with none the call ends. After the update brisvox is no longer in the media path, so the outcome
  reaches only the user's HTTP app (or brisvox via a status callback/poll: `GET` the child call, finding it needs the call list filter by parent: UNVERIFIED, not fetched).
  If the dial fails (`busy`/`no-answer`/`failed`), the caller returns to whatever the `action` URL says; to put the bot back, the `action` handler must return
  `<Connect><Stream>` again (new session with `resume` parameter, see 1.4). This is the documented way to support "transfer failed, bot continues".
- SIP REFER: `<Refer><Sip>sip:alice@example.com</Sip></Refer>` (https://www.twilio.com/docs/voice/twiml/refer, fetched 2026-10-08). "`<Refer>` is only supported on SIP call legs";
  PSTN and Voice SDK legs are not supported and return an error. So it applies only if the call reached Twilio via SIP (the SIP transport), not for PSTN numbers. Action parameters
  `ReferCallStatus`, `ReferSipResponseCode`, `NotifySipResponseCode`. Billing: not stated on that page (a Twilio help text seen in search says SIP REFER is a separately charged feature: UNVERIFIED).
- `<Redirect>` only changes the TwiML document ("All verbs after `<Redirect>` are unreachable and ignored"); not a transfer by itself.

#### 1.5.2 Announced / screened transfer (no conference): whisper on the agent leg
`<Dial><Number url="https://app/whisper">` runs TwiML "on the called party's leg after answer, before the parties are connected" (page 'number', fetched 2026-10-08; paraphrase of the page). Only
`<Play>`, `<Say>`, `<Gather>`, `<Hangup>` are supported there ("Other TwiML verbs aren't supported"); with `answerOnBridge` on `<Dial>` the caller keeps hearing ringing while it runs, and that TwiML cannot contain `<Dial>`.
Use: `<Say language="tr-TR" ...>` to read a short summary and a `<Gather numDigits="1">` "press 1 to accept"; `<Hangup/>` otherwise (the page: offer them a chance to decline using `<Gather>` and `<Hangup>`).
The `url` is a URL, so the user's HTTP app serves it (summary passed as a query string or looked up by `ParentCallSid`: design). Turkish `<Say>` voices (https://www.twilio.com/docs/voice/twiml/say/text-speech, fetched 2026-10-08):
Google Standard-A..E, Wavenet-A..E, Chirp3-HD (Aoede, Charon, Fenrir, Kore, Leda, Orus, Puck, Zephyr), Amazon Polly Neural `Burcu-Neural`, Polly Standard `Filiz`. `<Say>` text limits (same page): Basic 4000 characters,
Polly 3000 (excluding SSML tags), Google 5000 (including SSML tags, newlines and spaces).

#### 1.5.3 Warm transfer (bot or first agent briefs the target, then hands off)
Documented conference primitives:
- TwiML `<Conference>` (https://www.twilio.com/docs/voice/twiml/conference, fetched 2026-10-08): attributes `muted` (default false), `beep` (`true|false|onEnter|onExit`, default true),
  `startConferenceOnEnter` (default true; false = muted and hears hold music until a moderator with true joins), `endConferenceOnExit` (default false), `participantLabel` (unique in the conference, max 128 chars;
  duplicate -> error 16025, call not added), `waitUrl`/`waitMethod`, `maxParticipants` (<= 250), `record`, `region` (`us1,ie1,de1,sg1,br1,au1,jp1`), `jitterBufferSize` (`small|medium|large|off`, default large),
  `statusCallback`, `statusCallbackMethod` (the page is internally inconsistent about its default: text says POST, table says GET when no `waitUrl`: pass it explicitly), `statusCallbackEvent` (space-separated: `start end join leave mute hold modify speaker announcement`; the first participant's values win). The conference does not start until two participants join (and at least one has `startConferenceOnEnter` true);
  names are namespaced to the account; do not put personal data in the name.
  Callback parameters: `ConferenceSid, FriendlyName, AccountSid, SequenceNumber, Timestamp, StatusCallbackEvent` (`conference-start|end`, `participant-join|leave|mute|unmute|hold|unhold|modify|speech-start|speech-stop`, `announcement-end|fail`);
  participant events add `CallSid, Muted, Hold, Coaching, EndConferenceOnExit, StartConferenceOnEnter, ParticipantLabel`; `participant-leave` adds `ParticipantCallStatus`, `ReasonParticipantLeft`.
- Conference resource (https://www.twilio.com/docs/voice/api/conference-resource): `GET .../Conferences.json?FriendlyName=NAME&Status=in-progress` finds the `ConferenceSid` (`CF...`); `status` is `init|in-progress|completed`;
  conference rooms "are not directly created from the Programmable Voice API" (create with TwiML `<Dial><Conference>` or the Participants API); `POST .../Conferences/{Sid}.json` with `Status=completed` ends it or `AnnounceUrl=` announces to everyone. The page says "Starting from September 30th, 2026, the default [of the `Status` list filter] is `in-progress`" (already past on 2026-10-08): always pass `Status` explicitly. Docs recommend the `statusCallback` over polling.
- Participants (https://www.twilio.com/docs/voice/api/conference-participant-resource, fetched 2026-10-08; base `https://api.twilio.com/2010-04-01/Accounts/{AccountSid}/Conferences/{ConferenceSid}/Participants`):
  - Create: `POST .../Participants.json`; required `From`, `To`. Parameters (selection): `Label`, `CallerId`, `Timeout` (5-600, default 60, +5 s), `Muted`, `Beep`, `EarlyMedia` (default true), `StartConferenceOnEnter` (default true), `EndConferenceOnExit` (default false),
    `WaitUrl`, `Record`, `ConferenceStatusCallback`, `ConferenceStatusCallbackEvent` (default `start end`), `StatusCallback`/`StatusCallbackEvent` (call status), `MachineDetection`, `MachineDetectionTimeout` (default 30), `MachineDetectionSpeechThreshold`, `MachineDetectionSpeechEndThreshold`,
    `MachineDetectionSilenceTimeout`, `AmdStatusCallback`, `AmdStatusCallbackMethod` (default POST), `CallToken`, `Region` (`us1, us2, ie1, de1, sg1, br1, au1, jp1`). Response fields: `account_sid, call_sid, label, conference_sid, date_created, date_updated, end_conference_on_exit, muted, hold, status, start_conference_on_enter, coaching,
    call_sid_to_coach, queue_time (create only), uri`; participant `status`: `queued|connecting|ringing|connected|complete|failed`. The sample create response has `"status": "queued"`.
    Reconstructed request (the page's own curl example was too long to quote; this follows its fields):
    ```bash
    curl -X POST "https://api.twilio.com/2010-04-01/Accounts/$TWILIO_ACCOUNT_SID/Conferences/$CF_SID/Participants.json" \
    --data-urlencode "From=+15017122661" --data-urlencode "To=+905551112233" --data-urlencode "Label=agent" \
    --data-urlencode "EndConferenceOnExit=false" -u $TWILIO_API_KEY:$TWILIO_API_SECRET
    ```
  - Update: `POST .../Participants/{CallSid-or-Label}.json` (label must be percent-encoded). `Hold=true|false`, `HoldUrl`, `HoldMethod` (default GET), `AnnounceUrl`, `AnnounceMethod` (default POST), `Muted`, `BeepOnExit`, `EndConferenceOnExit`, `WaitUrl`, `Coaching`, `CallSidToCoach`.
    Documented samples (verbatim): hold: `--data-urlencode "Hold=true" --data-urlencode "HoldUrl=http://www.myapp.com/hold_music"`; announce: `--data-urlencode "AnnounceUrl=http://www.myapp.com/announce"`; both against
    `.../Conferences/CF.../Participants/CallSid.json`. Announce URL media types: the Conference resource page lists MP3, WAV or TwiML; the list of TwiML verbs allowed (`<Play>`, `<Say>`, `<Pause>`, `<Redirect>`) was NOT confirmed on the participant page: UNVERIFIED. Whether an announcement interrupts audio already playing is not stated. Long files delay playback; announcements of 30 minutes or longer can trigger a request timeout (conference and calls stay connected, participants hear "An application error has occurred").
  - Delete (remove the leg from the conference): `DELETE .../Participants/{CallSid}.json` -> HTTP 204, no body (the removed participant's call ends or continues per its TwiML: UNVERIFIED which, the page does not say; the Twilio blog tutorial instead ends a leg with Calls update `status: completed`).
  - List/fetch: list filters `Muted, Hold, Coaching, PageSize (default 50, max 1000)`; fetch accepts a Call SID or a label.
- Documented warm-transfer shape (Twilio TaskRouter "Call Control Concepts", https://www.twilio.com/docs/taskrouter/contact-center-blueprint/call-control-concepts, fetched 2026-10-08): (1) put the caller on hold (participant update `Hold=true` + `HoldUrl`), (2) create a new TaskRouter task for the transfer target
  (with the conference SID and original TaskSID) and, once assigned, call agent 2 into the conference with TwiML (the page's route is via TaskRouter, not a bare Participants create), (3) three-way call, (4) remove agent 1's leg via the Participants API, (5) complete the task. Cold-transfer variant on the same page: "it is not important for the addition of the new participant to be completed before the first agent call SID is removed"
  and "The conference and caller leg will stay up even if no one else is on the conference."
- Voice-AI example from Twilio (blog, published 2025-09-08, https://www.twilio.com/en-us/blog/developers/tutorials/product/warm-transfer-openai-realtime-programmable-sip): the caller sits in a conference
  (`<Conference startConferenceOnEnter="true" participantLabel="customer" endConferenceOnExit="true">`, conference named with the inbound `CallSid`), the AI is a SECOND conference participant (label "virtual agent") dialed over SIP, the human agent is added with Participants create (label "human agent", `earlyMedia: false`, `callToken`),
  a conference `join` status callback triggers removal of the AI leg by listing participants and updating that leg with `status: completed`. The article says it has no unit tests and that tests are needed before production; it does NOT state anything about a missing briefing/summary announcement or fragile error handling (an earlier version of this file claimed so; not supported by the page).
- Problem for brisvox (design analysis): with `<Connect><Stream>` the bot is not a conference participant; it is a WebSocket on the caller's own leg. Moving the caller into a conference by TwiML ends that flow (1.3), so the bot cannot brief the agent or stay in a three-way
  call. Three workable shapes:
  1. Consult-leg pattern (composed from documented primitives only; end-to-end UNVERIFIED): (a) update the caller's call with `<Play loop="0">` hold audio (or `<Conference>` alone with `waitUrl`); (b) create a second outbound call to the agent via Calls create with
     `Twiml=<Response><Connect><Stream url="wss://.../twilio/stream"><Parameter name="role" value="consult"/><Parameter name="parent" value="CALLER_CALL_SID"/></Stream></Connect></Response>` so a second brisvox session talks to the agent (briefing, "accept?" via speech or DTMF);
     (c) if accepted, update both calls with `<Dial><Conference>SAME_NAME</Conference></Dial>` (agent `endConferenceOnExit`, caller `endConferenceOnExit`), the consult stream ends; (d) if declined, update the caller back to `<Connect><Stream>` with `resume`. Needs `answered_by` handling on the agent leg (voicemail
     of the agent's phone must not "accept": use AMD from 1.7).
  2. Whisper screening (1.5.2): no conference, simplest, Twilio TTS reads the summary, agent accepts by DTMF. Good default for "warm".
  3. Bot as a conference participant from the start (Twilio blog shape): needs the bot reachable as a SIP/PSTN endpoint; not researched further.
- Tool contract (Proposal): `transfer_call { destination, mode: "cold" | "announced" | "warm", summary?, reason? }` as a SideEffect tool: never speculative, idempotency key per call+destination,
  one transfer per call. Result returned to the LLM: `{status: "initiated" | "failed", detail}`; after `initiated` the session ends (cold) or waits for the consult result (warm). The tool must (1) validate E.164 against an allow-list from config (prevents prompt-injection toll fraud), (2) have the
  bot finish its hand-off sentence and wait for the played-mark (the Twilio adapter notes in `twilio.md` "Ending the call"), (3) only then call the REST update.

### 1.6 Status callbacks (Twilio)
- Calls API: `StatusCallback` + `StatusCallbackEvent` (`initiated|ringing|answered|completed`, default `completed`); form-encoded request carrying the standard TwiML request parameters (https://www.twilio.com/docs/voice/twiml, fetched 2026-10-08): `CallSid, AccountSid, From, To, CallStatus` (page lists `queued, ringing, in-progress, completed, busy, failed, no-answer, canceled`; `initiated` is NOT in that list although it is a valid `StatusCallbackEvent` name), `ApiVersion, Direction`
  (`inbound|outbound-api|outbound-dial`), `ForwardedFrom, CallerName, ParentCallSid, CallToken`; status callbacks add `CallDuration`, `RecordingUrl/Sid/Duration`.
- `<Dial><Number statusCallback ...>` (page 'number'): adds `ParentCallSid`, `CallStatus`, `CallDuration` (completed only), `Timestamp` (RFC 2822, UTC), `CallbackSource` (`call-progress-events`), `SequenceNumber` (starts at 0; events may arrive out of order). Child calls are never queued.
- Conference callbacks: see 1.5.3. Stream callbacks (`statusCallback` on `<Stream>`): in `twilio.md`.
- Authenticity: same `X-Twilio-Signature` HMAC-SHA1 scheme as `twilio.md` ("Request validation"); the callback is HTTP(S), so the URL form is the normal one.

### 1.7 Answering Machine Detection (Twilio)
Docs: https://www.twilio.com/docs/voice/answering-machine-detection and `-faq-best-practices` (fetched 2026-10-08).
- Where AMD works (FAQ page): Calls API create, Participants API create, `<Dial><Number>`, `<Dial><Sip>`. It does NOT work on Elastic SIP trunking calls, `<Dial><Client>`, `<Dial><Conference>`, `<Dial><Queue>`. Changelog 2022-10-07: Dial and Participants AMD need an account with
  "Enhanced Programmable SIP Features". Inbound-call AMD: not offered (the pages show only outbound use; not stated explicitly).
- `MachineDetection`: `Enable` (returns as soon as the party is identified; use to connect humans / hang up on machines) or `DetectMessageEnd` (returns `human` immediately; for a machine only when the greeting ends, "usually indicated by a beep"; the mode for leaving voicemail).
- `AnsweredBy` values: with `Enable`: `machine_start`, `human`, `fax`, `unknown`; with `DetectMessageEnd`: `human`, `machine_end_beep`, `machine_end_silence`, `machine_end_other` (also a fallback when a machine was detected but `MachineDetectionTimeout` expired before the greeting ended), `fax`, `unknown` (timeout or initial-silence timeout).
  `machine_start` is not defined further on the page. FAQ: treat any `machine_end_*` as "machine, greeting finished" unless you must tell them apart.
- Timing parameters (Create Call / AMD pages): `MachineDetectionTimeout` default 30 s (AMD page range 3-59; the `<Dial><Number>` page says 3-60; the Create Call page lists no range: inconsistent, UNVERIFIED which is right; stay within 3-59), only relevant for `DetectMessageEnd` per the FAQ;
  `MachineDetectionSpeechThreshold` 1000-6000 ms default 2400 (higher = fewer false machine, slower machine detection); `MachineDetectionSpeechEndThreshold` 500-5000 ms default 1200 (higher helps short voicemails, delays human detection; lower risks false human);
  `MachineDetectionSilenceTimeout` 2000-10000 ms default 5000 (initial silence before `unknown`). FAQ tuning hints: greetings of individual humans (residential/mobile) typically under 1800 ms; business 1800-3000 ms; machines over 3000 ms; recommended values for residential/mobile callees SpeechThreshold 1800-2000 and SpeechEndThreshold 1400-1500; for business callees SpeechThreshold 1800-3000 and SpeechEndThreshold 1400-1500.
  "30 seconds is frequently not enough time" for business voicemail (FAQ); low `MachineDetectionTimeout` increases `unknown`.
- Sync vs async: default is synchronous: Twilio blocks the call until the verdict (the callee hearing silence for "several seconds" is our description, UNVERIFIED wording), then runs the TwiML; with `Url` it sends a webhook containing the decision (exact form-field name in that webhook: described as `AnsweredBy` in the AMD docs; the TwiML request-parameter page does not list it: UNVERIFIED). Async (`AsyncAmd=true`, Calls API): callee connected immediately,
  TwiML runs, result posted to `AsyncAmdStatusCallback` (+ `AsyncAmdStatusCallbackMethod`; AMD page says only POST, the Create page says GET|POST: UNVERIFIED which is right). Whether Participants API, `<Dial><Number>`, `<Dial><Sip>` are async only: UNVERIFIED (not stated on the AMD page; the 2022 changelog says AMD "will run in the background while the call connects" for those). Async callback fields: `CallSid`, `AccountSid`, `AnsweredBy`, `MachineDetectionDuration` (ms).
  The page shows no example payload (an earlier version of this file said it did); illustrative, constructed payload only: `CallSid=CA...&AccountSid=AC...&AnsweredBy=machine_end_beep&MachineDetectionDuration=4200`.
- Typical latency (FAQ): results "within ~4 seconds after the call was answered" with default settings (an average); webhook round trip is the most common cause of slowness. Accuracy: FAQ "close to 100% accurate in US destinations with default settings" for `DetectMessageEnd`; international accuracy "may be slightly reduced".
- Forked-stream interaction (AMD page, key for brisvox): async AMD uses one of the four forked audio streams per call; sync AMD uses none. The AMD page says Media Streams, SIPREC and Real-Time Transcription should be started "only after" Async AMD finishes, as indicated by the `AsyncAmdStatusCallback`. The stream overview page lists AMD among
  features sharing the four-track limit (that limit is stated for unidirectional streams); bidirectional streams are limited to one per call. Whether async AMD can run concurrently with a bidirectional `<Connect><Stream>` is therefore at best discouraged by the docs (UNVERIFIED if it errors).
- Recommended Twilio voicemail pattern for brisvox (Proposal built on the above): outbound Calls create with `MachineDetection=DetectMessageEnd` (sync), `MachineDetectionTimeout=45` (below 59), `MachineDetectionSilenceTimeout` default, `Url=https://app/voice` whose handler reads `AnsweredBy` and returns
  `<Connect><Stream url=...><Parameter name="answered_by" value="{AnsweredBy}"/></Stream></Connect>`; brisvox gets `CallInfo.metadata["custom.answered_by"]` at `start`, so no AMD plumbing is needed inside brisvox core. Humans: conversation starts about 0-4 s after pickup (sync-silence cost). Machines: wait for `machine_end_*`, then speak the message.
  Async variant to cut the silence: `AsyncAmd=true`, TwiML `<Pause length="N"/>` then `<Connect><Stream>` is NOT recommended (the docs say not to start a stream before the callback; a REST update on callback could start it, behaviour UNVERIFIED).
- Cost: $0.0075 per call (https://www.twilio.com/en-us/voice/pricing/us line "Answering machine detection ... $0.0075 per call", fetched 2026-10-08; the 2019 GA blog says charged when enabled and the called party answers, busy/failed not charged; 2022 changelog: "$0.0075 per completed call").
  Media Streams are listed at $0.0044 per minute and conference participants "Starting at $0.0018 / participant per min" on the same page.
- Language/countries: the AMD docs do not mention languages or Turkish (UNVERIFIED for Turkish voicemail greetings). A supported-country list was not found (the 2019 GA blog lists no countries; its example number is +1); the 2017 beta post said US and Canada only (old, from a search summary, UNVERIFIED); pricing pages for other countries list the AMD fee, which suggests availability but is not proof. Test with Turkish carriers (Turkcell, Vodafone, Turk Telekom voicemail) live before trusting it.

## 2. Telnyx Call Control

### 2.1 REST basics
- Base URL: `https://api.telnyx.com/v2` (OpenAPI server; docs examples `https://api.telnyx.com/v2/calls/{call_control_id}/actions/answer`). Docs: https://developers.telnyx.com/docs/voice/programmable-voice/sending-commands (fetched 2026-10-08). That page itself only covers base URL, auth header, and HTTP status meanings (200, 403, 404, 422); the facts below on `client_state`, `command_id`, response bodies and error codes come from the individual command pages (send-dtmf, transfer-call, bridge-calls, etc.) and the webhooks page.
- Auth: `Authorization: Bearer YOUR_API_KEY` (API key v2); JSON bodies (`Content-Type: application/json` in the docs' curl).
- `call_control_id` identifies the call leg to control (command pages: "Identifier and token for controlling the call"; the claim that it also routes to the node where the call lives, for lower latency, was NOT found on the fetched pages: UNVERIFIED). Every command takes optional `client_state` (Base64, echoed in all later webhooks) and `command_id` (send-dtmf page: Telnyx ignores any command with the same `command_id` for the same `call_control_id`; the webhooks page adds "within 60 seconds"; this is the idempotency key to use for transfer and DTMF).
- Success: `200 {"data":{"result":"ok"}}`. Errors: `422` with an error list (transfer page examples show `code`, `title`, `detail`; the `source`/`meta` members are UNVERIFIED); samples: `90015` invalid Call Control ID, `90018` call already ended, `90034` call not answered yet, `10004` missing parameter, `90045` "command can't be issued when media streaming is used"; generic failure `10007`.
  Dial lists `400, 422, 429 (rate limited), 500, 503`. Rate limit values: not found (UNVERIFIED).
- Commands exist (index page): `send_dtmf`, `send_sip_info`, `transfer`, `bridge`, `refer`, `hangup`, `streaming_start`, `streaming_stop`, `playback_*`, `speak`, `gather_*`, `record_*`, `answer`, `reject`, `fork_*`, `transcription_*`, `client_state_update`; conference commands `hold`, `unhold`, `join`, `leave`, `mute`, `unmute`, `play`, `speak`, `update`. No call-level hold command is listed (hold exists on conferences).

### 2.2 DTMF
Docs: https://developers.telnyx.com/api-reference/call-commands/send-dtmf (fetched 2026-10-08).
- Send: `POST https://api.telnyx.com/v2/calls/{call_control_id}/actions/send_dtmf`. Description: "Sends DTMF tones from this leg. DTMF tones will be heard by the other end of the call." Body: `digits` (required; "Valid digits are 0-9, A-D, *, and #. Pauses can be added using w (0.5s) and W (1s)", example `1www2WABCDw9`),
  `duration_millis` (100-500, default 250, "for how many milliseconds each digit will be played in the audio stream"), `client_state`, `command_id`. Response `{"data":{"result":"ok"}}`. No webhooks. No maximum number of digits stated (UNVERIFIED).
  ```bash
  curl -X POST https://api.telnyx.com/v2/calls/$CALL_CONTROL_ID/actions/send_dtmf \
    -H "Authorization: Bearer $TELNYX_API_KEY" -H "Content-Type: application/json" \
    -d '{"digits":"1www2","duration_millis":250,"command_id":"UUID"}'
  ```
  Whether this command is rejected (`90045`) or works while a media stream is active on the same leg: not documented (UNVERIFIED, live-test item 4). Separate commands exist for SIP INFO (`send_sip_info`, not fetched) and conference participants (`/v2/conferences/{id}/actions/send_dtmf`, not fetched). `send_digits_on_answer` (max 64 chars, `0-9 A-D w W * #`) on Dial/Transfer; a `to` may end with `,digits` (`+18004247767,200`); an explicit `send_digits_on_answer` overrides digits appended to `to`.
- Receive over the stream: `{"event":"dtmf","stream_id":..,"occurred_at":..,"sequence_number":"5","dtmf":{"digit":"1"}}` (digits `0-9 * # A B C D`; may arrive out of order; order by `occurred_at`; `occurred_at` is optional in the schema; full schema in `telnyx.md`).
- Receive as webhook: `call.dtmf.received` exists with `data.event_type = "call.dtmf.received"`, `data.payload.digit` (string), plus `call_control_id, call_leg_id, call_session_id, client_state, connection_id, from, to` (source: official SDK model `telnyx-python` `types/call_dtmf_received.py`, read 2026-10-08).
  The Voice API webhooks page describes it as "A DTMF digit was received", fired when the caller presses keys during Gather using audio or Gather using speak (paraphrase; the exact earlier-quoted sentence was not verified verbatim), and the Gather command docs list it as an expected webhook; it is NOT documented as a general always-on DTMF event. For a streamed bot, use the `dtmf` stream frame.
- Mute: `mute_dtmf` (`none|both|self|opposite`) on Transfer/Bridge: DTMF is not passed through but DTMF webhooks still fire (Bridge page wording).

### 2.3 Transfer on Telnyx
- Transfer: `POST https://api.telnyx.com/v2/calls/{call_control_id}/actions/transfer` (https://developers.telnyx.com/api-reference/call-commands/transfer-call, fetched 2026-10-08). Only `to` required (DID or SIP URI; `;secure=true|srtp|dtls`; `,digits` suffix allowed).
  Selected body fields: `from` (default the original call's `to`; E.164), `from_display_name` (<=128), `audio_url`/`media_name` (WAV/MP3 played to the destination on answer, before bridging; mutually exclusive), `send_digits_on_answer`, `early_media` (default true), `timeout_secs` (default 30, 5-600), `time_limit_secs` (default 14400, 30-14400),
  `park_after_unbridge` (`self`), `answering_machine_detection` (default `disabled`; enum below) + `answering_machine_detection_config`, `custom_headers`, `sip_headers` (only `User-to-User`), `sip_transport_protocol`, `sip_region`, `preferred_codecs`, `media_encryption`, `target_leg_client_state`, `client_state`, `command_id`, `webhook_url`(+method), `mute_dtmf`, `record*`.
  Behaviour: bridges the caller (Leg A) to a NEW leg (Leg B) to `to`. Webhooks: `call.initiated`, `call.bridged` (to Leg B), `call.answered` or `call.hangup`, and AMD events when requested. On a failed transfer the docs say a `call.hangup` for Leg B is sent; the original call remains active and can receive further commands (incl. a transfer to an alternate destination).
  Timeouts/limits produce `call.hangup` with `hangup_cause` `timeout` or `time_limit`. After the bridge ends, the leg hangs up unless `park_after_unbridge: "self"`. Response `{"data":{"result":"ok"}}`.
  ```bash
  curl -X POST https://api.telnyx.com/v2/calls/$CALL_CONTROL_ID/actions/transfer \
    -H "Authorization: Bearer $TELNYX_API_KEY" -H "Content-Type: application/json" \
    -d '{"to":"+905551112233","from":"+908501234567","timeout_secs":30,"command_id":"UUID"}'
  ```
- Media stream interplay: the transfer page lists `90045` (media streaming in use) among its 422 examples and says nothing else about streaming. The same 90045 example ("This command can't be issued when media streaming is used.") appears on the streaming_start page. Which commands are blocked by an active stream is NOT stated (UNVERIFIED, live-test item 4).
  If `transfer`/`bridge` is rejected with `90045`, issue `POST .../actions/streaming_stop` first (`stream_id` optional; omitted = all streams of the call; webhook `streaming.stopped`), wait for it, then transfer; on failure, `streaming_start` again. Whether the WebSocket gets a `stop` frame after `streaming_stop`: the streaming_stop page does not describe it; the WebSocket reference says Telnyx sends `stop` "when it stops media streaming for the call" (the media-streaming guide: when the call ends, after the `streaming.stopped` webhook): treat the exact ordering as UNVERIFIED. Streams: "Currently only one streaming/fork operation is supported per call" (media-streaming page).
- Bridge two legs: `POST .../calls/{call_control_id}/actions/bridge` with body `call_control_id` (the other leg; mutually exclusive with `queue`/`video_room_id`), `park_after_unbridge`, `hold_after_unbridge`, `play_ringtone`, `ringtone`, `mute_dtmf`, `prevent_double_bridge`, `record*`, `client_state`, `command_id`. Webhooks `call.bridged` for both legs. Errors incl. `90034` not answered.
- Dial a second leg linked to the first: `POST https://api.telnyx.com/v2/calls` with required `connection_id`, `to`, `from`; `link_to` (call control id to share a session with), `bridge_intent` (requires `link_to`; overwrites `from` with the linked call's `from`), `bridge_on_answer` (requires `link_to`), `prevent_double_bridge`, `park_after_unbridge` (requires `link_to`); response `data`: `call_control_id, call_leg_id, call_session_id, is_alive (always false for dial), client_state, record_type:"call", ...`. Dial also takes the streaming fields (`stream_url`, `stream_track`, `stream_bidirectional_mode`, `stream_auth_token`, ...) so a CONSULT leg can be streamed to a second brisvox session (same consult pattern as Twilio 1.5.3 shape 1). Fully documented parameters are in the Dial page; shape not live-tested.
- Conference: `POST /v2/conferences` (`call_control_id`, `name` required; `start_conference_on_create` default true; `hold_audio_url`/`hold_media_name`; `max_participants` 2-800 default 250 (the page's own error examples conflict on this range, UNVERIFIED); `duration_minutes`; conferences expire after all leave or 4 hours; status `init|in_progress|completed`; `end_reason` `all_left|ended_via_api|host_left|time_exceeded`),
  `POST /v2/conferences/{id}/actions/join` (`call_control_id` required; `hold`, `hold_audio_url`, `mute`, `start_conference_on_enter` (default false), `end_conference_on_exit`, `soft_end_conference_on_exit`, `supervisor_role` `barge|monitor|none|whisper`, `whisper_call_control_ids`, `beep_enabled`), `POST /v2/conferences/{id}/actions/hold` (`call_control_ids` empty = all; `audio_url`/`media_name`).
  Webhooks (create-conference page) `conference.created`, `conference.participant.joined|left`, `conference.ended`, `conference.recording.saved`, `conference.floor.changed`. A warm transfer on Telnyx is therefore: conference the caller, hold the caller, dial and join the agent, brief, unhold, hang up the bot leg. As with Twilio, the bot-as-stream sits on the caller's leg, whether it survives conference join is UNVERIFIED.
- SIP REFER: `POST .../actions/refer` with `sip_address` (required SIP URI), `client_state`, `command_id`, `custom_headers`, `sip_auth_username/password`, `sip_headers` (`User-to-User` only). Webhooks `call.refer.started`, `call.refer.completed`, `call.refer.failed` (the earlier-noted payload field `sip_notify_response` was NOT found on the refer page: UNVERIFIED). Whether the call must be answered: not stated (`90034` is a listed generic error).
  The SIP trunking "External transfers" page describes blind (SIP REFER, "Immediate transfer without announcement") and attended transfer (hold, dial, announce, then REFER) for SIP endpoints, with a required `Diversion` header: that page concerns SIP-trunk endpoints and is not directly the Call Control stream bot case.
- Telnyx's own AI Assistant product has a warm-transfer-acceptance feature (consult leg, built-in `complete_transfer` tool, 2-minute decision timeout, `private|shared` context): it is a different product; recorded only as design reference (https://developers.telnyx.com/docs/inference/ai-assistants/warm-transfer-acceptance).
- Price line seen: "Call transfer | $0.10 per invocation" (https://telnyx.com/pricing/voice-api, fetched 2026-10-08); which commands count as a "transfer invocation" is UNVERIFIED.

### 2.4 AMD on Telnyx
Docs: https://developers.telnyx.com/docs/voice/programmable-voice/answering-machine-detection, Dial and Transfer pages (fetched 2026-10-08).
- Enable with `answering_machine_detection` on Dial (outbound) or Transfer. (An earlier version of this file quoted the page as saying "outbound call creation or inbound call transfer"; that wording was NOT found on the AMD guide, Dial or Transfer pages: UNVERIFIED.) Modes: `disabled` (default), `detect`, `detect_beep`, `detect_words`, `greeting_end`, `premium` ("recommended" on the guide), `premium_ios_call_screening_detection`.
  | Mode | Webhooks | `result` values |
  |---|---|---|
  | `detect` | `call.machine.detection.ended` | `human`, `machine`, `not_sure` ("Recommended to treat as if human answered") |
  | `detect_beep` | detection.ended; `call.machine.greeting.ended` only if a beep is detected | greeting: `beep_detected` |
  | `detect_words` | detection.ended; greeting.ended on beep or after a 30 s beep-detection window | greeting: `beep_detected` or `not_sure` (timeout) |
  | `greeting_end` | both; Transfer page: greeting.ended when the greeting ends "with a beep or silence" | greeting: `ended` |
  | `premium` | `call.machine.premium.detection.ended`, `call.machine.premium.greeting.ended` | detection: `human_residence`, `human_business`, `machine`, `silence`, `fax_detected`, `not_sure`; greeting: `beep_detected`, `no_beep_detected` |
  | `premium_ios_call_screening_detection` | adds `call.machine.premium.call_screening.detected` | `screening`; greeting `prompt_ended` |
  Payload (SDK models `call_machine_detection_ended.py` and `call_machine_greeting_ended.py`, read 2026-10-08): `data.event_type`, `data.id`, `data.occurred_at`, `data.record_type:"event"`, `data.payload.{call_control_id, call_leg_id, call_session_id, client_state, connection_id, from, to, result}` (all optional in the SDK);
  `result` Literal for detection.ended: `human|machine|not_sure`; for greeting.ended: `beep_detected|ended|not_sure`. The premium payload schemas were not fetched (UNVERIFIED beyond the values in the guide).
  Order: `call.initiated` -> `call.answered` -> detection result -> greeting result; callee hang-up gives `call.hangup` at any point. Premium ordering: if a beep is heard before `detection.ended`, the greeting webhook is sent first (UNVERIFIED on the current guide fetch, which shows the general order and the iOS order only).
- Config `answering_machine_detection_config` (Dial page defaults): `total_analysis_time_millis` 3500, `after_greeting_silence_millis` 800, `between_words_silence_millis` 50, `greeting_duration_millis` 3500, `initial_silence_millis` 3500, `maximum_number_of_words` 5, `maximum_word_length_millis` 3500, `silence_threshold` 256, `greeting_total_analysis_time_millis` 5000,
  `greeting_silence_duration_millis` 1500, beep tuning (`beep_detection_profile` `both|freq_only`, `beep_min_frequency_hz` 200-4000, `beep_max_frequency_hz`, `beep_min_tone_duration_millis` 50-5000, `beep_spectral_*`), `prompt_end_timeout_millis` (Dial/Transfer pages: default 5000, 1000-120000; the guide says 30000 default; iOS screening only). For `premium` only `total_analysis_time_millis` and `greeting_duration_millis` apply.
  The AMD guide quotes premium default analysis timeout 30 s (result `not_sure` on expiry) while the Dial and Transfer pages list 3500 for the config default: inconsistent (UNVERIFIED which applies to premium).
  The field descriptions on the Dial page make the heuristics explicit (useful design reference for an in-engine detector): initial silence over `initial_silence_millis` (3500 ms) = machine; a greeting longer than `greeting_duration_millis` (3500 ms) = machine; more than `maximum_number_of_words` (5) words = machine; a single word longer than `maximum_word_length_millis` (3500 ms) = machine; silence over `after_greeting_silence_millis` (800 ms) after a greeting = human. (These are descriptions of config fields, not a full algorithm spec.)
- Cost: standard AMD $0.002 per call, premium $0.0065 per call (https://telnyx.com/pricing/voice-api, fetched 2026-10-08); Media Streaming over WebSockets $0.0035 per minute; Voice API outbound "$0.002 per minute + the SIP Trunking fee".
- Language/Turkish: not documented in the AMD guide (UNVERIFIED). Telnyx streaming/AMD webhooks need a Call Control App connection and a public webhook (UNVERIFIED on fetched pages; design assumption).

### 2.5 Telnyx webhook mechanics (needed for transfer and answering-machine detection if brisvox consumes them)
Docs: https://developers.telnyx.com/docs/voice/programmable-voice/voice-api-webhooks (fetched 2026-10-08).
- Envelope: `data.{record_type:"event", event_type, id, occurred_at, payload}` and `meta.{attempt, delivered_to}`.
- Signature headers: `Telnyx-Signature-Ed25519` (ED25519 signature) and `Telnyx-Timestamp` (Unix timestamp); the earlier claim "compatible with the Standard Webhooks specification" is NOT on the fetched webhooks page: UNVERIFIED. Verification steps are on a separate Webhook Fundamentals page (not fetched: UNVERIFIED).
- Delivery: 2xx = ack; 3xx followed up to 3 redirects; 408, 429, 5xx retried; other 4xx not retried; "webhooks may be delivered more than once" -> dedupe on `data.id`; `webhook_timeout_secs` 0-30; failover URL `webhook_event_failover_url`; ack "within a few seconds" and process asynchronously (the "few seconds" wording UNVERIFIED on the latest fetch; retry count/interval not specified on the page). Per-call webhook override via `webhook_url` (+ `webhook_url_method`) on commands; `webhook_urls`/`webhook_retries_policies` (max 5 retries, total <= 60000 ms) are on the Dial page.
- Correlate with `call_leg_id`; `call_session_id` groups related legs (e.g. both sides of a transfer). Ordering guarantees: none documented.

## 3. Brisvox design for DTMF, transfer and voicemail (Proposal)

### 3.1 DTMF
- Inbound: already produced by the Twilio adapter (`dtmf` -> `WireEvent::Dtmf`) and by the Telnyx adapter (`dtmf.digit`, order by `occurred_at`). The design adds: delivery to the turn manager/flows and to tools as an event with timestamp; a debounce/collector (`collect_digits { max, terminator '#', timeout }` tool or flow node) with an explicit timeout (engine rule); DTMF while the bot speaks counts as barge-in only if configured; DTMF is NOT run through STT.
- Outbound: transport capability flag `can_send_dtmf`: Telnyx true (REST `send_dtmf` with `command_id`); Twilio `false` over the stream, with two opt-in fallbacks (REST update + reconnect, or in-band synthesis); loopback/WebSocket transports: custom message. `send_dtmf` tool = SideEffect, idempotency key per (call, digits, sequence), digits validated against `[0-9A-D*#wW]` and a length limit (Twilio `SendDigits` 32, Telnyx `send_digits_on_answer` 64; `digits` max unspecified).
- Crates: no crate needed. Hand-write the two-tone generator (a few lines, `no_std`-style, reusable buffer) and a Goertzel check in tests. crates.io check (API, 2026-10-08, re-checked in fact-check): `dtmf` 0.1.5 (MIT, decoder, last release 2022-11-04, 9820 total downloads: stale), `dtmf_tones` 1.0.2 and `dtmf_table` 1.1.4 (MIT, frequency tables only, no audio generation per the repo README; `dtmf_tones` repo shows 21 commits, 0 stars, no releases; new, tiny, not adopted).
  `twiml-rust` 0.1.0 exists (TwiML generator, 180 downloads) but is not needed: TwiML strings here are tiny and must be XML-escaped by a small helper (escape `&`, `<`, `>`, `"`). reqwest 0.13.5 (MIT OR Apache-2.0, released 2026-09-08) is already a workspace dependency for REST calls.

### 3.2 Transfer
- Port: `CallControl` trait (provider-agnostic): `transfer(target, mode, ctx) -> TransferOutcome`, implemented for Twilio (`TwilioCallControl`, REST) and Telnyx (`TelnyxCallControl`, REST), each with timeouts on every HTTP call, a bounded retry policy that never retries a possibly-applied command (use GET status first on Twilio; `command_id` on Telnyx), and credentials in `Secret`.
- State machine on the session: `Active -> TransferPending(epoch) -> HandedOff | Failed(reason) -> Active|Ended`. While pending, the bot stops generating new LLM turns; the farewell audio must be fully played (mark) before the REST call.
- Twilio cold: REST update with `<Dial>` (1.5.1). Twilio announced: `<Dial><Number url=whisper>` (1.5.2). Twilio warm: consult-leg pattern (1.5.3, shape 1) or conference; all UNVERIFIED end to end, hence the pending live check.
- Telnyx cold: `transfer`; warm: dial linked leg (+ streamed consult) and `bridge`, or conference; remember `streaming_stop` before commands that fail with `90045`.
- Safety (engine invariant: SideEffect never speculative, never twice): `transfer_call` runs only after the model's tool call finishes (not speculatively), idempotency key = hash(call id, target); destination allow-list or country allow-list in config; log the target redacted.

### 3.3 Voicemail strategy
Detection layers, in order of authority:
1. Provider AMD when brisvox originates the call (Twilio `DetectMessageEnd` sync via the user's `Url` webhook -> `answered_by` custom parameter; Telnyx `greeting_end` or `premium` via the Dial request + webhooks). The result must reach the session (Twilio: custom parameter at `start`; Telnyx: webhook or `client_state`).
2. In-engine confirmation (always on, cheap): watch the first seconds of the callee audio with VAD/STT: a single long continuous utterance over the Telnyx-documented 3500 ms, many words, or voicemail phrases in the STT text (Turkish phrases such as "mesaj bırakın", "bip sesinden sonra", "sesli mesaj" are a design suggestion, not from a fetched source) overrides `human`. A beep cue is only reliably available from the provider (`machine_end_beep`, `beep_detected`); an in-engine beep detector would be a new DSP component (not researched).
3. Fallback when AMD says `unknown`/`not_sure`: treat as human (Telnyx guide: `not_sure` "Recommended to treat as if human answered" for standard AMD; for premium the docs give no recommendation); Twilio gives no recommendation (UNVERIFIED), `fax` -> hang up.

Leave-message flow (state machine):
- `Dialing -> Answered -> Classifying(≤ amd_timeout) ->` human: normal conversation; machine: `WaitGreetingEnd(≤ greeting_timeout) -> Leaving -> Done`.
- `WaitGreetingEnd` ends on Twilio `machine_end_beep|machine_end_silence|machine_end_other` or Telnyx `call.machine.greeting.ended` (`beep_detected` or `ended`); on a Twilio `Enable` verdict `machine_start` do NOT speak yet. On Telnyx `detect_words`, `not_sure` after the 30 s window means speak anyway or hang up (policy flag).
- `Leaving`: play ONE fixed message (pre-rendered or synthesized once, Turkish text from config), barge-in disabled, no LLM turn, no STT-driven interruption; wait for the utterance-end mark (`MarkPlayed`) with a timeout, then hang up. Message length bounded (config, e.g. 20-30 s; voicemail systems cut off: carrier/system limit UNVERIFIED). The message must not reference data the callee did not consent to (compliance note, not a fetched fact).
- Provider-side alternative for the message itself: Twilio `<Say language="tr-TR">` (voices in 1.5.2) or `<Play>`; Telnyx `speak`/`playback_start` (not fetched: UNVERIFIED Turkish voices). Prefer brisvox's own TTS for voice consistency and Turkish quality.
- Metrics/logging: AMD verdict, `MachineDetectionDuration` (Twilio, ms), time from answer to verdict, time from greeting end to first message audio; counted per call outcome (`human|machine_message_left|machine_hangup|unknown|fax`).

## 4. Open items to settle with live tests (need accounts, numbers, public wss URL)
1. Twilio: effect of REST Update (new TwiML) on an active `<Connect><Stream>`: does Twilio send `stop`, close the socket, in what order; does a new `<Connect><Stream>` in the update TwiML start immediately; what does the caller hear in the gap; `<Connect action>` firing on server close.
2. Twilio: `<Play digits>` tone duration/level and whether the far-end IVR reliably accepts it; does in-band G.711 DTMF sent on the stream reach an IVR (tone length/gap needed).
3. Twilio: Update Call `Twiml` length limit; error codes/status for updating an ended call; whether `Url` update triggers a signed request with the usual `X-Twilio-Signature`.
4. Telnyx: whether `send_dtmf`, `transfer`, `bridge` are blocked (`90045`) while a media stream is active; which exact commands; effect of transfer on the stream (`stop` frame?); whether `streaming_stop` + `transfer` + `streaming_start` is quick enough.
5. Twilio consult-leg and conference warm transfer end to end (hold/unhold, `Hold` participant update on a leg created by `<Dial><Conference>`, drop of the bot leg, ConferenceSid discovery by `FriendlyName`).
6. AMD: sync `DetectMessageEnd` latency distribution on Turkish carriers and Turkish voicemail greetings (Twilio and Telnyx), false-human/false-machine rate, `machine_end_other` frequency, behaviour with mobile "arama yönlendirme" announcements; whether async AMD can coexist with a bidirectional stream; whether Turkey is a supported AMD destination at all.
7. Twilio `AnsweredBy` form-field name in the `Url` webhook and the `AsyncAmdStatusCallbackMethod` GET support (docs disagree).
8. Telnyx: premium AMD payload schemas and default `total_analysis_time_millis`; `call.dtmf.received` outside Gather.
9. Billing: Twilio parent+child leg billing for `<Dial>` transfers; Telnyx "Call transfer $0.10 per invocation" scope; conference participant minutes.
10. Rate limits for the REST endpoints on both providers.

## Sources (all fetched 2026-10-08)
Twilio
- https://www.twilio.com/docs/usage/requests-to-twilio (and .md)
- https://www.twilio.com/docs/voice/api/call-resource (and .md, full pages incl. Update/Delete sections)
- https://www.twilio.com/docs/voice/tutorials/how-to-modify-calls-in-progress (and .md)
- https://www.twilio.com/docs/voice/twiml (request parameters), /twiml/play, /twiml/dial, /twiml/number, /twiml/conference, /twiml/refer, /twiml/redirect, /twiml/connect, /twiml/stream, /twiml/say (+ /say/text-speech for Turkish voices and limits)
- https://www.twilio.com/docs/voice/media-streams (DTMF statements, four-track limit, stopping a bidirectional stream), https://www.twilio.com/docs/voice/media-streams/websocket-messages
- https://www.twilio.com/docs/voice/api/conference-resource, /conference-participant-resource
- https://www.twilio.com/docs/taskrouter/contact-center-blueprint/call-control-concepts (cold/warm transfer sequences)
- https://www.twilio.com/en-us/blog/developers/tutorials/product/warm-transfer-openai-realtime-programmable-sip (2025-09-08)
- https://www.twilio.com/docs/voice/answering-machine-detection, /answering-machine-detection-faq-best-practices
- https://www.twilio.com/en-us/blog/answering-machine-detection-generally-available (2019-07-17), https://www.twilio.com/en-us/changelog/answering-machine-detection-is-now-available-for-dial-and-post-to-participants-calls (2022-10-07)
- https://www.twilio.com/en-us/voice/pricing/us (AMD, Media Streams, conference participant lines)
- Web search summaries (not primary text): AMD supported countries (2017 beta US/Canada), SIP REFER billing, update-during-stream behaviour (confirmed undocumented).
Telnyx
- https://developers.telnyx.com/api-reference/call-commands/send-dtmf (.md), /transfer-call, /bridge-calls, /sip-refer-a-call, /streaming-start, /streaming-stop, /hangup-call, /dial (full page read up to ~100k chars)
- https://developers.telnyx.com/api-reference/conference-commands/create-conference, /join-a-conference, /hold-conference-participants
- https://developers.telnyx.com/docs/voice/programmable-voice/sending-commands, /voice-api-commands-and-resources, /voice-api-webhooks, /answering-machine-detection, /media-streaming
- https://developers.telnyx.com/api-reference/websockets/stream-call-media-over-websocket (stream frames, dtmf)
- https://developers.telnyx.com/docs/voice/sip-trunking/features/external-transfers, https://developers.telnyx.com/docs/inference/ai-assistants/warm-transfer-acceptance
- https://github.com/team-telnyx/telnyx-python (raw `src/telnyx/types/call_dtmf_received.py`, `call_machine_detection_ended.py`, `call_machine_greeting_ended.py`; official SDK models used for webhook field names because the callback doc pages render no schema)
- https://telnyx.com/pricing/voice-api
- crates.io API: reqwest 0.13.5, dtmf 0.1.5, dtmf_tones 1.0.2, dtmf_table 1.1.4, twiml-rust 0.1.0, search `dtmf`; https://github.com/jmg049/dtmf_tones
- Not fetched: Telnyx Webhook Fundamentals (signature verification steps), `send_sip_info`, `speak`, `playback_start`, Telnyx premium AMD callback pages (render no schema), Twilio Calls list filters (child-call lookup), Twilio SIP REFER billing, per-account rate limits for both providers.

## Fact-check log
Fact-checked 2026-10-08: 135 claims checked, 20 corrected. Every cited Twilio and Telnyx page, the four Telnyx SDK model files and the crates.io/GitHub entries were re-fetched on 2026-10-08. Corrections made in place above:
1. Twilio Create Call: `Twiml` is the parameter ignored when both `Twiml` and `Url` are given (file previously said `Url` was ignored).
2. `SendDigits` vs `MachineDetection`: page says `MachineDetection` "will be ignored"; wording fixed.
3. Update Call description quote "Modifies an active call..." not verbatim on the page; replaced by the actual wording.
4. Parent/child call quotes reworded to the actual page text ("entirely new Call instance", "is considered in-progress by Twilio", "you must use `Status=completed`").
5. `<Stream>` TwiML page: the quoted "doesn't say what happens..." was not a page quote; "one Stream per Call" belongs to the media-streams overview page.
6. `CallStatus` values: `initiated` is not in the page's list.
7. Twilio AMD page has no example async payload; marked as constructed.
8. "Participants / Dial Number / Dial Sip are async only" not stated on the AMD page; marked UNVERIFIED (changelog says "run in the background").
9. Conference resource quote corrected ("Conference rooms are not directly created from the Programmable Voice API").
10. Twilio blog (2025-09-08): only "no unit tests" is stated; the claims of no briefing/summary and fragile error handling removed.
11. Participants announce URL verb list (`<Play>`, `<Say>`, `<Pause>`, `<Redirect>`) not confirmed; marked UNVERIFIED.
12. Telnyx 2.1: `client_state`/`command_id`/error codes are not on the sending-commands page (re-attributed to command pages); node-routing claim and `source`/`meta` error members marked UNVERIFIED; 60-second `command_id` window added.
13. Telnyx "compatible with the Standard Webhooks specification" not found on the webhooks page; marked UNVERIFIED.
14. Telnyx AMD "outbound call creation or inbound call transfer" wording not found; marked UNVERIFIED.
15. crates.io `dtmf` downloads: 3108 corrected to 9820.
16. `call.dtmf.received` verbatim quote replaced by a paraphrase of the webhooks page.
17. Telnyx refer `sip_notify_response` payload field not on the refer page; marked UNVERIFIED.
18. Twilio FAQ threshold hints: 1800-2000/1400-1500 are for residential/mobile callees (business: SpeechThreshold 1800-3000); clarified.
19. TaskRouter warm-transfer step 2 reworded (new TaskRouter task, then TwiML into conference); Twilio `MachineDetectionTimeout` range note (Create page lists none) added.
20. Telnyx: `stop` frame after `streaming_stop` ordering marked UNVERIFIED; premium event-ordering claim marked UNVERIFIED; added conference `max_participants` conflict, `conference.recording.saved`, join default, `prompt_end_timeout_millis` default conflict (5000 vs 30000).

Remaining UNVERIFIED items: Twilio REST rate limits/idempotency/error body; Update Call `Twiml` max length and error codes for ended calls; effect of REST update on an active bidirectional stream; whether `<Connect action>` fires on server close; `AnsweredBy` field name in the `Url` webhook; `AsyncAmdStatusCallbackMethod` GET support; `MachineDetectionTimeout` range (3-59 vs 3-60); AMD countries/Turkish support (both providers); SIP REFER and `<Dial>` billing; Twilio `machine_start` definition; whether DTMF tones are also in the `media` payload; Telnyx rate limits, `call_control_id` node routing, `send_dtmf` max digits and stream interplay (`90045` scope), premium AMD payload schemas and default timeout, premium event ordering, `sip_notify_response`, Standard Webhooks compatibility, Webhook Fundamentals signature steps, `call.dtmf.received` outside Gather, `$0.10` transfer invocation scope, Turkish voices for Telnyx `speak`; ITU DTMF frequency table except keys 5 and 8; carrier voicemail length limits.
