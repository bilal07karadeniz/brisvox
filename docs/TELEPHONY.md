# Telephony: DTMF, call transfer, answering machines

Protocol facts live in `docs/providers/call-control.md` (Twilio and Telnyx REST, fetched and
fact-checked 2026-10-08). This page is the design and the wiring: what was built, where it lives,
how an application connects it, and what is still unverified. Anything marked UNVERIFIED is a
provider behavior the documentation does not state; the code handles the possibilities
defensively and the live tests settle them.

## 1. Where the code lives

| Piece | Location | Depends on |
|---|---|---|
| Vocabulary (`DtmfString`, `PhoneNumber`, `TransferRequest`, `AnsweredBy`, `TelephonyOps`, errors) | `brisvox_agent::telephony` | core |
| Tools `transfer_call`, `send_dtmf`, `end_call`; `TelephonyPolicy` | `brisvox_agent::telephony::tools` | the vocabulary only |
| DTMF in: note, `DtmfCollector` | `brisvox_agent::telephony::dtmf` | core |
| Voicemail: `voicemail_flow`, `VoicemailDriver`, `voicemail_hint` / `looks_like_voicemail` | `brisvox_agent::telephony::voicemail` | flows |
| REST call control: `CallControl`, `TwilioCallControl`, `TelnyxCallControl`, AMD config and parsers, `BoundCall` | `brisvox_transport::call_control` (cargo feature `call-control`, off by default) | agent (vocabulary), reqwest, rustls |
| A stream that ends on purpose and comes back (Twilio `StreamResumer`, Telnyx `StreamStopper`) | `brisvox_transport::twilio` (`registry.rs`), `brisvox_transport::telnyx` | transport only |
| The binary's wiring: telephony tools, the live-call hub, warm-transfer and AMD callbacks, voicemail handling | `brisvox_server::tools::telephony`, `brisvox_server::callbacks`, `brisvox_server::runner` (cargo feature `call-control`) | all of the above |

**Decision: a module of the transport crate, not a new crate.** It is the same kind of thing as
the Twilio media adapter (a provider protocol for a call), it shares the crate's error and
feature conventions, and a new crate would add a manifest and nothing else. The agent tools depend
only on the `TelephonyOps` trait object, never on transport; transport depends on agent for the
shared vocabulary, which the crate graph allows (agent is below transport). The TLS setup (rustls
with `ring`, webpki roots) is a 15-line copy of `brisvox_providers::net::tls_config` so
transport does not depend on its sibling, the providers crate. If a third consumer appears, move
it to core.

## 2. DTMF in

```text
provider "dtmf" message
  -> WireEvent::Dtmf (transport adapter: Twilio dtmf.digit, Telnyx dtmf.digit)
  -> TransportEvent::Dtmf { digit, at }              (session)
  -> DataFrame::Dtmf(DtmfFrame { epoch: 0, digit, at })   (engine pump, lane "dtmf")
  -> TurnDriver re-tags epoch, forwards downstream
  -> LLM runner: DtmfCollector::push(digit, at)
       -> Collected { digits, reason: Terminator | MaxDigits | Timeout }
       -> context.push(dtmf_message(&digits, language))   // a user message
       -> next LLM round
```

DTMF never goes through STT. The runner arms a timer for `DtmfCollector::next_deadline()` on the
injected clock and calls `poll(now)` when it fires, so a half-typed entry is flushed (the engine
rule: every wait has a timeout). The note is `[The caller pressed the keys: 1 2 3 #]` (Turkish:
`[Arayan şu tuşlara bastı: 1 2 3 #]`); digits are the only variable content, so it cannot carry
an injection. Whether a key press interrupts the bot is the application's interruption policy;
the helpers only shape text. The runner side (feeding the collector, arming the timer) is up to the engine.

Twilio only sends `dtmf` on bidirectional streams, inbound direction, with no duration or
timestamp; whether the tone audio is also in the `media` payload is UNVERIFIED. Telnyx sends
`dtmf.digit` and an optional `occurred_at` (order by it; frames may arrive out of order).

**Ordering is the transport adapter's job.** `DtmfFrame` carries only `at` (receive time), not
`occurred_at`, and the collector only sees frames in the order the pump delivers them. The Telnyx
media adapter must therefore emit `dtmf` frames in `occurred_at` order (a short reorder
buffer keyed by `occurred_at`/`sequence_number`) and stamp `at` accordingly; this is an open
requirement for that task, not something the agent crate can repair. The collector is robust
against a non-monotonic `at` (it never moves the inter-digit deadline backwards).

**A lone terminator is an entry.** `#` (the default terminator) or `*` pressed with nothing
pending ("press # to skip") completes an entry of its own, `CollectReason::LoneTerminator`, whose
only key is the terminator, so a caller who skips or goes back reaches the model.
`DtmfCollectorConfig::report_lone_terminator = false` drops it instead.

## 3. DTMF out

| Transport | How | Status |
|---|---|---|
| Telnyx | `POST /calls/{id}/actions/send_dtmf` with `digits`, `duration_millis` (default 250), `command_id` | works; may be refused with `90045` while a media stream is active (UNVERIFIED which commands): reported as `Error::StreamActive`, the stream is **not** stopped because the bot continues after the tones |
| Twilio | cannot be sent over the media stream (documented). Opt-in route: update the call with `<Play digits="..."/>` followed by a new `<Connect><Stream>` (`ResumeStream`) | needs `TwilioConfig::resume_stream` **and** `TwilioConfig::adapter_resumes_session = true`; the update ends the old stream, the new one has a new `streamSid` and `custom.resume = "dtmf"` in the call metadata, there is an audio gap; heavy, use for deliberate IVR navigation. The shipped Twilio media adapter handles the old stream's end as a hang-up and does not map the new stream back to the live session, so the capability is **off by default**: without both settings `capabilities().send_dtmf` is false and `send_dtmf` is `Unsupported`, nothing is sent (the model is told "not available on this call", not "sent" while the conversation is lost). Register the tool from `BoundCall::capabilities()`: `TelephonyPolicy::with_dtmf(bound.capabilities().send_dtmf)` |
| WebSocket / loopback | no REST route | `Unsupported` |

In-band synthesis of the two-tone waveform as `media` frames is a documented alternative whose
far-end behavior is carrier-dependent and unverified; it is future work (`call-control.md` 1.4).

`send_dtmf` tool: SideEffect, digits validated against `[0-9A-Da-d*#wW]{1,32}` (schema pattern
and `DtmfString`), lowercase `a-d` normalized, at least one real key. The `Debug` form of a digit
string prints only its length (it may be a PIN).

## 4. Transfer

### Tool

`transfer_call { destination, mode?, summary?, reason? }`, SideEffect.

**Transfer by an out-of-process agent.** The agent process of `docs/AGENT_PROTOCOL.md`
sends a `transfer` command (E.164 number, `cold` or `warm`): it is trusted like the operator
(it holds the application logic), so the number comes from it and not from the destination list.
It runs through the same call binding as the tool (`CallOps`), including the hand-off after a
cold transfer and the warm-transfer gate (`[telephony] warm`, callback listener); the call gets
this binding even when the definition names no telephony tool.

- **The model never produces a number.** `destination` is an `enum` of the *names* in
  `TelephonyPolicy` (`sales`, `duty_manager`), each mapped to an E.164 number in configuration.
  Prompt injection cannot reach a premium-rate number. No destination configured: the tool is not
  registered. Numbers appear in no prompt, tool result or log (`PhoneNumber` debug-prints
  `+90******33`).
- `mode`: `cold` (default) or `warm` (only offered when `TelephonyPolicy::with_warm(true)`).
- **One transfer per call.** A second call is refused while one runs or was accepted. A definite
  failure (`Rejected`, `Unavailable`, `InvalidArgument`) releases the claim so the model can try
  another destination; `OutcomeUnknown` keeps it and tells the model not to repeat it.
- **The hand-off sentence is played first.** The tool awaits `TelephonyOps::settle_speech()`
  (bounded by `settle_timeout`, default 8 s, on the injected clock), then asks for the transfer.
  The application implements `settle_speech` from the engine's play-out marks
  (`TransportEvent::MarkPlayed`); `BoundCall::with_settle` takes the function.
- **The tool timeout covers the whole chain.** `TelephonyPolicy::effective_transfer_timeout()` is
  `settle_timeout + provider_budget + 2 s` (default 8 + 40 + 2 = 50 s). The provider budget is
  four sequential requests at the transport's default 10 s deadline, the longest chain (Telnyx:
  refused conference command, stream stop, repeat, dial; Twilio: park, create agent leg, roll
  back). Set `provider_budget` to `requests * request_timeout` when the REST deadline is changed;
  an explicit `transfer_timeout` is used as given (a shorter one can answer the model "timed
  out, do not repeat" for a transfer that then succeeds).
- The idempotency key (`ToolCallCtx::idempotency_key().digest()`) reaches every request as the
  Telnyx `command_id`; the engine's store replays the recorded result for a repeated call id.
- Result: `{status: "initiated", mode, destination, summary_delivered, instruction}`. After
  `initiated` a cold transfer ends the bot's part of the call.

### Cold transfer

- Twilio: update the call with
  `<Response><Dial callerId=".." answerOnBridge="true" timeout="30" action="..">+90..</Dial></Response>`
  (`callerId`/`action` only when configured). Twilio ends the call after the `<Dial>`; the dial
  outcome reaches only the `action` URL of the user's HTTP app.
- Telnyx: `POST .../actions/transfer` with `to`, optional `from`, `timeout_secs`, `command_id`. If
  refused with `90045` the binding issues `streaming_stop` and repeats the command once under the
  command id `...r1` (the refused one was not applied). The stream is not restarted if the retry
  fails (`stop_stream_on_conflict = false` disables the whole retry). A retry that fails after the
  stop succeeded is `Error::StreamStopped { cause }` (an unknown outcome as far as the model is
  concerned): the bot's stream is gone, so the tool must not say "nothing was done, the call
  continues with you". The transfer tool keeps its claim and tells the model not to repeat; the
  application hangs the caller up or moves them on. A failing `streaming_stop` itself is reported
  as it is (the stream is then still up).
- The host's media adapter must not take the stream that ended because of that `streaming_stop`
  for the end of the call. `TelnyxConfig::on_stream_stop` is a hook (`StreamStopHook`) called with
  the call right before the binding sends `streaming_stop` (once per conflict), so the host can
  mark that stream as stopped on purpose in the media adapter (the adapter's own API lives in
  `docs/providers/telnyx.md`); the binding itself has no dependency on the adapter.

### Warm transfer (conference based)

Twilio (`TwilioCallControl::transfer_warm`), all on one background task (see "Cancellation"):

1. Update the caller's call: `<Dial><Conference startConferenceOnEnter="false" endConferenceOnExit="true" beep="false" participantLabel="caller" [waitUrl]>brisvox-<key></Conference></Dial>`. The caller waits (hold music or `conference_wait_url`); the bot's stream ends (UNVERIFIED how, see 7).
2. Create a call to the destination (`From` = `caller_id`, required) whose TwiML is only a silent
   `<Pause>` of `agent_accept_window_secs` (default 120, in pauses of at most 60 s). **The agent
   leg is not in the conference and nothing is read to it.** `StatusCallback` events
   `initiated ringing answered completed` when configured, and AMD parameters when
   `agent_leg_amd` is set (the sync verdict is readable with `TwilioCallControl::call_status`
   once the leg is answered, `answered_by`; with `async_amd` it arrives at the callback URL).
3. **`complete_warm` is the accept gate.** When the application has seen the agent leg answered
   by a person (AMD verdict, status callback), `complete_warm` updates the agent leg with
   `[<Say language=..>summary</Say>]<Dial><Conference startConferenceOnEnter="true" endConferenceOnExit="false" ..>brisvox-<key></Conference></Dial>`:
   the agent hears the summary (the caller does not), joins, and the two meet. A voicemail on the
   agent's phone is never joined to the caller and never hears the summary: if the application
   does nothing, the leg ends when the pause ends. (A `<Gather>` accept prompt, as call-control.md
   1.5.2 suggests for the no-conference shape, would need a webhook, which brisvox does not
   serve; the REST step makes the application's decision explicit instead.) The agent leg must be
   answered: an update of a ringing call is rejected by Twilio.
4. `abort_warm` (no answer, declined, voicemail) ends the agent leg (`Status=completed`) and
   then resumes the caller through `ResumeStream`; it stops at the first error so a retry repeats
   it safely, and returns where the caller was left (`Resumed`, or `Parked` without a resume
   stream). The agent leg counts as ended when the update succeeds, when Twilio answers 404, or
   when it answers another 4xx (not 401/403) and a read of the leg (`GET`) shows a final status
   (`completed`, `busy`, `failed`, `no-answer`, `canceled`): a leg that already ended on its own
   (no answer, busy) is the usual case, and the error Twilio gives for updating a finished call
   is UNVERIFIED, so the leg is read instead of trusting one status code. A queued or ringing leg
   is ended with `Status=canceled` (the status Twilio documents for it) after `completed` was
   refused. If the leg is still live (or unreadable) the original refusal is returned and the
   caller is **not** resumed behind it; every retry repeats the same sequence.
5. If step 2 fails definitively (4xx), the caller is put back on the bot through `ResumeStream`
   (`resume = warm_failed`); `Error::WarmFailed` says where the caller was left. If its
   outcome is **unknown** (timeout, dropped connection, 5xx; also a failed step 1) the caller is
   **not** resumed behind a leg that may exist: the error reports `Parked` and carries a recovery
   handle (`WarmFailed::transfer`: the conference, no agent leg) that `BoundCall` keeps for
   `abort_warm`. The possibly existing leg only rings and then ends after its pause (nothing
   joins it to the empty conference); its SID is unknown, look it up in the console if it
   matters. The whole TwiML (including the later join, with the summary) is checked against the
   4000-character limit and the caller id before step 1, so a bad request touches nothing.

Telnyx (`TelnyxCallControl::transfer_warm`; needs `connection_id` and `caller_id`):

1. `POST /conferences` with the caller's leg (and `hold_audio_url`; **without it the caller hears
   silence** while waiting): needs `data.id` in the response (the response shape is not on the
   fetched pages; a missing id is `Error::Protocol`). A `90045` here triggers the same
   stop-and-retry as the cold transfer (tested: `conferences`, `streaming_stop`, `conferences`).
   If the retry fails after the stop succeeded, the error is `WarmFailed { Parked }` wrapping
   `Error::StreamStopped`, without a recovery handle (no conference id is known): the caller's
   stream is gone and the application must act.
2. `POST /calls` (dial) with `link_to` the caller's leg over `connection_id`, optional AMD fields.
   Failure (or a dial answer without a leg id) leaves the caller parked in the conference:
   `WarmFailed { Parked }` with a recovery handle (the conference id, no agent leg) that
   `BoundCall` keeps.
3. When the application sees `call.answered` for the agent leg and the AMD verdict says a person,
   it calls `complete_warm`, which joins the agent leg to the conference. Until then the agent
   leg is not in the conference.
4. `abort_warm` hangs up the agent leg (if there is one) and reports `CallerState::Parked`
   **always**: the stream was stopped to make the conference command possible and cannot be
   restarted from here, so there is no way back to the bot. The application must hang the caller
   up (or move them on) itself; the state is returned instead of an `Ok(())` that would hide it.
   An outcome-unknown dial may have created a leg whose id is unknown; it is never joined (the join
   is explicit), but an answered one stays up until its far end hangs up.
5. The summary cannot be delivered (the `speak` / `playback_start` commands were not fetched):
   the report says `Briefing::NotSupported` and the tool result `summary_delivered: false`.

`BoundCall` and the warm-transfer handle: the REST steps *and* the recording of their result run
on one spawned task, so a tool task that is aborted after the caller was moved cannot lose the
handle. `BoundCall::warm_transfer()` returns it (after a half-failed transfer: the recovery
handle). `complete_warm()` forgets the transfer once the provider accepted the join;
`abort_warm()` forgets it only after the provider confirmed the abort, so after a timeout or a
5xx the call can be repeated, and returns the `CallerState`. Without a transfer, `abort_warm`
does nothing and reports `Unchanged`.

Both providers deliver the later events (agent answered, declined, hung up) as callbacks that
the transport crate does not serve (scope boundary in `twilio.md`). The application's HTTP app
receives them and calls `BoundCall::complete_warm()` / `abort_warm()`; Twilio status callbacks
carry `CallSid`, `CallStatus`, Telnyx webhooks `data.event_type` (`call.answered`,
`call.hangup`, ...), deduplicated on `data.id`. **The server binary serves the endpoints that
finish a transfer** (section 5, "Warm transfers in the binary"): the application's HTTP app
calls them instead of linking `BoundCall`.

### Rules every REST call follows

Deadline on the injected clock around send + body (default 10 s, connect 3 s; a server that
sends headers and then stalls, or streams an endless body, ends at the deadline or at the 64 KiB
body cap, both tested; a test with a clock that runs 1000 times faster than real time proves the
deadline comes from the injected `Clock` and not from `tokio::time`); no redirects (a 302/307
answer is an error after exactly one request, tested), no proxy; `http://` only on loopback; credentials as
`Secret`, header values sensitive, never in an error or `Debug`; **the request URL (account SID,
call id, conference id) is stripped from transport errors** (`reqwest::Error::without_url`),
because the error text reaches the model as a tool result and the logs; provider error text is stripped of control characters, cut to 200 characters (tested with a
5000-character reply on both providers) and dropped for 401/403. **No automatic retry** except the one Telnyx `90045` case: a timeout or a
dropped connection may mean the command was applied (`Error::outcome_unknown()`), and the tool
reports "do not repeat". After a Twilio timeout use `TwilioCallControl::call_status` before
anything is repeated.

### Cancellation

Warm transfers run their steps on their own tokio task, and `BoundCall::transfer` records the
resulting handle on that task too: dropping the caller's future (the tool engine's backstop, a
session end) does not leave the caller half-moved between the REST steps, and does not lose the
handle that `complete_warm` and `abort_warm` need (a test drops the tool future after step 1 and
finds the handle). Every step has a deadline, so the task is bounded. What the REST sequence
cannot do is keep the *caller* off the conference when the whole process dies between steps;
that is the application's recovery (the conference name `brisvox-<key>` is deterministic).
SideEffect tools are not dropped by the engine for a timeout or an interruption anyway.

## 5. Wiring an application

```rust
let control = Arc::new(TwilioCallControl::new(twilio_config, clock.clone())?);
let call = CallRef::new(call_sid)?;                       // CallInfo.metadata["call_sid"]
let bound = Arc::new(BoundCall::new(control, call).with_settle(settle_fn));

let ops: Arc<dyn TelephonyOps> = bound.clone();
let caps = bound.capabilities();
let policy = TelephonyPolicy::new()
    .destination("sales", "+905551112233", "the sales team")?
    .with_warm(caps.warm_transfer)
    .with_dtmf(caps.send_dtmf);             // false for Twilio unless adapter_resumes_session
let mut registry = ToolRegistry::new();
register_telephony_tools(&mut registry, &ops, &policy, &clock)?;

// after each response was played:
if bound.hangup_requested() { /* send TransportCommand::Stop after the farewell mark */ }
// from the HTTP app's callbacks:
bound.complete_warm().await?;                       // the agent is a person: join
let left = bound.abort_warm().await?;               // CallerState::{Resumed, Parked, ..}
```

`end_call` records the request (`hangup_requested()`); the model says one goodbye sentence next
and the application ends the call after that sentence's utterance-end mark, as in the Twilio
adapter's "Hang-up" rule. `BoundCall::hangup` (REST) is for transports whose stream cannot end the
call.

### In the server binary

`brisvox-server` wires all of this itself (cargo feature `call-control`,
`brisvox_server::tools::telephony`), from `BRISVOX_CALL_CONTROL=twilio,telnyx` and the
definition's `[telephony]` section (`docs/OPERATIONS.md`, `docs/CONFIG.md`):

- **Which call.** The transport session records the call's `Started` info
  (`TransportSession::call_started`); the tools bind to the carrier call the first time they act
  (Twilio `call_sid`, Telnyx `call_control_id`, chosen by the `transport` metadata). A call on a
  listener without a configured carrier (generic websocket, RTVI, SIP) has the same tools:
  `transfer_call` and `send_dtmf` answer `Unsupported` ("not available on this call"), `end_call`
  ends the call through the transport. So a definition's tool set does not depend on the
  listener.
- **`settle_speech`** follows the call's engine events (`EngineEvents`, the RTVI bridge's ring
  when it has one): it returns once the bot is not speaking and produced nothing for 1 s, bounded
  by the policy's `settle_timeout` (8 s).
- **`end_call`**: the runner waits for the end of the next bot speech (the farewell; at most
  10 s), hangs up through the carrier when the call has one (bounded at 12 s, abandoned when the
  session is cancelled; a failure is logged) and stops the call. A farewell the model speaks in
  the same response as the `end_call` call counts: the engine reports the end of a response's
  speech once per response, after its tool loop, so after the tool ran. A response that says
  nothing ends its speech at once, so a silent `end_call` hangs up right away.
- `send_dtmf` is offered only when every configured binding can send tones: Telnyx always, Twilio
  when the server also hosts the Twilio listener (below).

#### Streams that end on purpose

Two REST operations end the bot's media stream while the call goes on: Twilio's TwiML update
(DTMF and the first step of a warm transfer replace the call's TwiML, which ends the
`<Connect><Stream>`; the new TwiML starts a *new* stream) and Telnyx's `streaming_stop` before
a transfer retry (error `90045`). Left alone, the media adapter reads the old stream's `stop`
or close as the caller hanging up, ends the engine's session and the tool call in flight with
it. The binary wires both sides:

- **Twilio.** `HostSettings::stream_links()` gives the call control the Twilio listener's
  `StreamResumer` (a late-bound cell, filled when the listener binds) and the stream URL, the
  listener's `BRISVOX_TWILIO_PUBLIC_URL` as `wss://`. The Twilio binding then has a
  `ResumeStream` (`resume` parameter `dtmf` or `warm_failed`), `adapter_resumes_session = true`
  (so `send_dtmf` is offered) and `on_stream_restart`, which announces the stream's end to the
  adapter right before the REST update. The adapter (`brisvox_transport::twilio`, module docs
  "Streams that end on purpose and come back"): for 30 s after the announcement a `stop`, a
  close frame or a vanished socket of that call is the host's doing; the engine gets no
  hang-up, its session is parked, and what it says meanwhile (up to a minute of audio) is held
  for the new stream, a barge-in `clear` dropping it. A stream of the same `callSid` whose
  `start` carries the `resume` parameter is handed to the parked session (it may arrive before
  the old stream finished closing). A session nobody resumed within the announced time (DTMF
  30 s; a warm transfer: the agent leg's accept window + ring time + 60 s, at most 15 min) ends
  as a disconnect; a `resume` stream nobody expects is refused (1008). The resumed stream passes
  the same signature check and the same admission gate as any other: at the call limit it is
  refused like a new call (keep headroom in `BRISVOX_MAX_SESSIONS`).
- **Telnyx.** `on_stream_stop` announces the `streaming_stop` to the Telnyx listener's
  `StreamStopper` (`Wiring the Telnyx stream stop`, section 7): the close is not a hang-up, the
  session stays up (at most `STOPPED_LINGER`, 120 s) and the retried transfer completes.

After a **cold transfer** returns, the call has left the bot: the runner stops the session without
hanging the call up (it would drop the transferred call). The same for a completed warm
transfer.

#### Warm transfers in the binary

`[telephony] warm = true` in the definition offers `transfer_call` with `mode = "warm"`, when
the callback listener runs (`BRISVOX_CALLBACK_ADDR`) and every configured binding can do one
(Twilio: `BRISVOX_TWILIO_CALLER_ID`; Telnyx: `BRISVOX_TELNYX_CALLER_ID` and
`BRISVOX_TELNYX_CONNECTION_ID`); otherwise the server warns at startup and offers cold transfers
only. The tool starts the transfer (caller to a conference; receiving party called). The rest
is two requests to the callback listener (`docs/OPERATIONS.md`, "Callback listener"):

- `POST /v1/calls/{carrier}/{call}/warm/complete`: the receiving party accepted. The listener
  calls `BoundCall::complete_warm` (Twilio: the agent leg reads the summary and joins the
  conference), then the runner stops the bot's session without a hang-up.
- `POST /v1/calls/{carrier}/{call}/warm/abort`: the receiving party did not answer, declined or
  is a machine. `BoundCall::abort_warm` ends their leg; on Twilio the caller goes back to the bot's
  stream (the parked session continues) and the bot says so (a fixed line in the agent's
  language, `en` or `tr`); on Telnyx there is no way back (`CallerState::Parked`) and the call is
  hung up.

The decision to accept belongs to the application: it watches the receiving leg (Twilio
`BRISVOX_TWILIO_STATUS_CALLBACK`, AMD, its own phone logic; Telnyx webhooks) and calls one of the
two. A transfer nobody finishes is cleaned up when the call's session ends: the server aborts it
and hangs the caller up, so the caller is never left in a conference alone. Telnyx's session
survives the missing stream for `STOPPED_LINGER` (120 s): finish a Telnyx transfer within that.

### Caller identity for the agent process

An agent process (`docs/AGENT_PROTOCOL.md`) can be told who is calling, off by default
(`[agent_link] caller_identity = true`, `docs/CONFIG.md`; withheld while phone numbers are
redacted, `docs/PRIVACY.md`). Where the server finds each value:

| carrier | `carrier_call_id` | `from` / `to` |
|---|---|---|
| Twilio | `CallInfo.metadata["call_sid"]` (the `CallSid` of the stream's `start`) | the stream parameters `from` and `to` (`custom.from`, `custom.to`; `From` and `To` also work) |
| Telnyx | `call_control_id` | `start.from` and `start.to` |
| SIP | the `Call-ID` (`CallInfo::call_id`) | the user parts of the INVITE's `From` and `To`, only when made of `0-9 A-Z a-z + * # . _ -` (the headers are the caller's own, unauthenticated text; anything else drops the field) |

The Twilio `start` message has no numbers, so the TwiML that opens the stream has to pass them:
your voice webhook receives `From` and `To` and answers with

```xml
<Response><Connect><Stream url="wss://voice.example.com/twilio/stream">
  <Parameter name="from" value="+15551230001"/>
  <Parameter name="to" value="+15551230002"/>
</Stream></Connect></Response>
```

with the real values filled in per call (the voice webhook of `examples/phone-receptionist` does
exactly this). Stream parameters are not covered by the signature of
the stream URL, so treat them as hints, like a caller ID. Without the parameters an opted-in
agent still gets the `CallSid`.

## 6. Answering machines

*(The server binary's handling is at the end of this section.)*

### Strategy (layers, in order of authority)

1. **Provider AMD** when brisvox originates the call. Twilio: `MachineDetection=DetectMessageEnd`
   (the mode for leaving a message; `Enable` returns early with `machine_start`), sync (the call
   waits for the verdict, about 4 s typical) or `AsyncAmd` with a callback. Telnyx:
   `answering_machine_detection` `greeting_end` (or `detect_beep`, `premium`), results as
   webhooks. **Leaving a message needs the end of the greeting**: only Twilio `DetectMessageEnd`
   and Telnyx `greeting_end` / `premium` send it (`AmdConfig::twilio_provides_greeting_end()`,
   `telnyx_provides_greeting_end()`). Twilio `Enable` and Telnyx `detect` end at `machine_start` /
   `machine`; with them use `VoicemailPolicy::detect_only()` (hang up on machines; with
   `speak_when_greeting_end_uncertain` a fixed `hint_wait` of 6 s instead of waiting out the 30 s
   greeting timeout). A test feeds a Telnyx `detect` event to both policies. `AmdConfig` is the neutral configuration with documented ranges;
   `twilio_params()` and `telnyx_fields()` render it for the originating request (the same is used
   for the warm-transfer agent leg). The Telnyx mapping of the timing fields is an approximation
   (UNVERIFIED equivalence, documented on `telnyx_fields`).
2. **Results to the session**: `parse_twilio_amd_callback(form body)` and
   `parse_telnyx_amd_event(json body)` return an `AnsweredBy`; Twilio synchronous AMD can instead
   put `AnsweredBy` into a stream `<Parameter>` (`CallInfo.metadata["custom.answered_by"]`) so no
   plumbing is needed in core. Telnyx detection `machine` maps to `MachineStart` (greeting still
   playing); the greeting result (`beep_detected` / `ended`) maps to `MachineEnd*`.
3. **In-engine hint** `voicemail_hint(text, speech_duration)` returns a strength:
   `HintStrength::Phrase` for voicemail phrases in English and Turkish ("leave a message", "bip
   sesinden sonra", "mesaj bırakın", "sesli mesaj", ...) and `HintStrength::Length` for a greeting
   longer than 3.5 s with more than 5 words (Telnyx's documented thresholds; `looks_like_voicemail`
   is the plain yes/no of the two). It is fed to
   `VoicemailDriver::on_voicemail_hint(strength, now)` and only ever upgrades toward "machine",
   but the strengths are not equal, because a live person's "hello, this is the Smith residence,
   how can I help you" is also long:
   - An `Unknown` verdict settles as a machine if a hint of **either** strength came before it.
   - A `Human` verdict is overridden only by a **phrase**: when the phrase arrives within
     `VoicemailPolicy::human_upgrade_window` (10 s, zero disables) after the verdict, or when it
     was heard up to that long **before** the verdict (the speech-to-text final can beat the
     provider's verdict; Twilio `DetectMessageEnd` answers `human` at once for a short greeting
     that goes on with "leave a message after the beep"). A length hint never changes a `Human`
     verdict, in either order.
   - The driver then waits `hint_wait` (6 s) for the greeting to end and speaks.
   The window and the phrase specificity are the guard against reading the message to a real
   person. A machine verdict already given is never reversed. The phrase lists are suggestions,
   to be tuned on real Turkish voicemail.
4. **Fallback** `VoicemailPolicy::on_unknown` (`TreatAsHuman` by default, as Telnyx advises for
   `not_sure`; `TreatAsMachine`; `HangUp`) and `speak_when_greeting_end_uncertain`.
   `TreatAsMachine` (an `Unknown` verdict or the classify timeout) does **not** speak at once:
   an `Unknown` usually is a detection timeout, so the greeting may still be playing. Like the
   hint path it waits `hint_wait` (6 s, ends early on a `MachineEnd*` verdict), then speaks, or
   hangs up when `speak_when_greeting_end_uncertain` is off.
5. **Future work**: an in-engine beep detector (DSP on the callee audio) and greeting-length
   analysis without provider help; per-country AMD availability and Turkish carrier behavior are
   UNVERIFIED (live test).

### Leave-message flow

`voicemail_flow(&VoicemailConfig, registry)` builds a `Flow` with these nodes; the
verdicts come from the application, so transitions are applied by
`FlowManager::transition_to` as `VoicemailDriver` directs, not by tool results.

```text
classify  (respond_immediately off: the bot is silent)
   Human          -> human          (respond_immediately: greets; end_call -> farewell -> EndCall)
   MachineStart   -> wait_greeting  (silent, barge-in off)
   MachineEnd*    -> leave_message  (Reset; pre actions in order:
                                     Custom("barge_in", {"enabled": false}), Say(message), EndCall)
   Fax            -> hang up
   Unknown/timeout-> VoicemailPolicy
```

The action contract is documented on `flows::Action`: a node's action list runs strictly in
order, `Say` finishes when its utterance-end mark was played (bounded by the runner), and
`EndCall` hangs up after everything queued before it was played, so `EndCall` after `Say` in
`leave_message` hangs up after the message's utterance-end mark (with the runner's own timeout),
not immediately. There is no model turn and no tool,
so nothing the callee says can change the fixed message. After the mark, call
`VoicemailDriver::message_played()`. `VoicemailDriver::deadline()` is the instant the runner must
call `on_timeout` at (classification 45 s, greeting 30 s by default). Outcomes for metrics:
`human`, `machine_message_left`, `machine_hangup`, `fax`, `unknown`.

Compliance: the message is spoken and may be recorded; put nothing in it the callee did not
consent to hear on a recording. Voicemail systems cut recordings at an unknown limit; keep it
short (20 to 30 seconds).

### Voicemail in the binary

An agent with `[telephony.voicemail]` (`docs/CONFIG.md`) handles the verdicts of calls the
carrier's detector classifies. The binary does not originate calls: the application places the
call with the detector on (`AmdConfig::twilio_params` / `telnyx_fields`) and points the
carrier's stream at the binary. The verdict reaches the call two ways:

1. **Stream parameter.** Twilio's synchronous `MachineDetection` answers before the call's TwiML
   runs; the application's TwiML generator puts the result in the stream:
   `<Parameter name="answered_by" value="machine_end_beep"/>` (any Twilio `AnsweredBy` value). It
   is read at the call's start (`CallInfo.metadata["custom.answered_by"]`).
2. **Callback.** `POST /v1/amd/twilio` (`AsyncAmdStatusCallback`: `CallSid`, `AnsweredBy`) or
   `POST /v1/amd/telnyx` (the webhook), on the callback listener. The call is found by the
   carrier's call id. An asynchronous detector can answer before the media stream connected: a
   verdict for a call that is not live waits for it (up to 60 s, at most 256 waiting, answered
   `202 queued`) and is applied when the call starts.

Each call with a verdict gets a `VoicemailDriver` (no driver for a call without one, so inbound
calls are never touched) with the policy of the definition (`on_unknown`, `detect_only`; the
other timings are the driver's defaults: greeting timeout 30 s, `hint_wait` 6 s, human upgrade
window 10 s). Its steps: a machine whose greeting ended (`machine_end_*`) -> the bot speaks the
message (a line through the engine's say input, cutting anything the bot was saying), and when
the speech ended (bounded at 90 s) the call is hung up through the carrier; `fax`, or a machine
with `detect_only`, or `on_unknown = "hang_up"` -> hung up at once; a person -> nothing, the
conversation goes on. The callee's final transcripts are also fed to `voicemail_hint`: a
voicemail phrase ("leave a message after the beep") upgrades a `human` or `unknown` verdict, as
in the driver's rules above. A line that cannot be spoken (the engine takes none, as with an
out-of-process agent) hangs the call up instead.

Limits: the bot is not silenced while the verdict is pending (the flow's `classify` node does
that in the library, the binary has no flows). Arrange for the verdict to come first (the stream
parameter) or make the agent wait for the callee to speak; and the barge-in settings of the
definition apply to the message, so a beep that the VAD takes for speech can cut it (the
message is spoken after the greeting ended, when the line should be quiet).

## 7. Unverified, guessed around, and live tests

Items marked here are provider behavior not stated in the fetched documentation. The code is
defensive about each.

1. **Twilio: effect of a REST update on an active `<Connect><Stream>`** (order of `stop`, socket
   close, whether a new stream in the update TwiML is allowed). The DTMF route and warm-transfer
   rollback depend on it. The Twilio adapter treats a stream ending right after an announced
   REST update as expected, not as a hang-up: it handles `stop`, a close frame and a
   vanished socket alike, and a new stream that arrives before the old one closed. Whether
   Twilio really starts the new stream, and how soon, is settled by the live test. Update `Twiml`
   length limit: the 4000 characters of the Create page are applied.
2. **Twilio**: `<Play digits>` tone duration and level at the far end; error codes when updating
   an ended call (a 404 is mapped to `CallGone`, anything else is `Rejected` with the code);
   `AnsweredBy` field name in the sync-AMD webhook; whether async AMD may run beside a
   bidirectional stream; AMD in Turkey.
3. **Telnyx**: which commands `90045` blocks while streaming; effect of `transfer` on the stream
   (`stop` frame?); the response shape of `POST /conferences` and `POST /calls` (`data.id`,
   `data.call_control_id` are assumed and validated); `command_id` accepted by Dial; `hold_audio_url`
   behavior for a lone participant; premium AMD payload schemas.
4. **Both**: rate limits, billing of parent and child legs, REST error body shapes beyond the
   documented samples (both Twilio `code`/`message` and `error_code`/`error_message` are read).

Live checks still pending (need accounts, numbers, a public `wss://` URL): DTMF both ways and
IVR; cold and warm transfers on both providers, stream interplay; AMD on Turkish carriers,
voicemail message flow.

### Wiring the Telnyx stream stop
When the Telnyx media adapter and Telnyx call control run in the same process, set
`TelnyxConfig::on_stream_stop = Some(StreamStopHook::from_stopper(server.stream_stopper()))`.
The 90045 retry path then announces its `streaming_stop` to the adapter first, so the stream
close it causes is neither reported as a caller hang-up nor answered with a REST hang-up
(test `the_call_control_stop_hook_marks_the_stream_as_stopped_on_purpose`). The server binary
does this itself (section 5): `HostSettings::stream_links()` carries the Telnyx
listener's `StreamStopper` to the binding (test `telnyx_stream_stop_of_a_transfer_retry_is_not_a_hang_up`
in `crates/brisvox-server/tests/telephony_extras.rs`).

## 8. Security

What an eavesdropper or a stranger can do on each telephony transport, and what the server does
about it. (See `docs/SECURITY.md` for the cross-cutting review.)

- **Twilio.** The listener is plaintext: terminate TLS in front of it. Every upgrade must carry a
  valid `X-Twilio-Signature` (HMAC-SHA1 over the public URL with the account's Auth Token). A fixed
  URL has one constant signature that can be replayed by anyone who saw it once, so the stream URL
  is per call: `BRISVOX_TWILIO_STREAM_SECRET` makes every URL end in a signed, single-use,
  expiring segment bound to the call's SID (`docs/providers/twilio.md`, "Per-call stream URLs").
  `BRISVOX_TWILIO_ALLOW_STATIC_URL=1` is the legacy escape hatch and logs a warning. The used-set
  of segments is per process: after a restart, or against another instance, a captured signed URL is
  replayable until its `exp` (at most 1 h; 120 s for URLs the server mints). Migrating from the
  fixed URL: see "Migrating from the fixed URL" in `docs/providers/twilio.md`.
  `start.customParameters` are not signed; do not put authority in them.
- **Telnyx.** Telnyx signs nothing. The path and the optional `stream_auth_token` are the only
  checks, so a Telnyx listener bound to a non-loopback address refuses to start without
  `BRISVOX_TELNYX_STREAM_TOKEN` (set the same value as `stream_auth_token` in the stream
  request); `BRISVOX_INSECURE_LISTENERS=1` overrides it with a warning. Keep the path unguessable
  too (`BRISVOX_TELNYX_PATH`).
- **SIP.** UDP and TCP signaling and plain RTP are cleartext, and the digest `Authorization`
  protects only who may call, not who can listen. For a trunk across the internet turn on **SIPS
  and SRTP** ("SIP over TLS and SRTP" below): `BRISVOX_SIP_TLS_ADDR` with a certificate
  and key, and `BRISVOX_SIP_SRTP=required`. Without them run it on a **private trunk, a VPN or
  behind an SBC** that terminates TLS/SRTP toward the carrier. The listener refuses to start
  without at least one inbound check: `BRISVOX_SIP_ALLOWED_SOURCES` (a CIDR ACL of the carrier's
  signaling networks) and/or `BRISVOX_SIP_AUTH_USER` with `BRISVOX_SIP_AUTH_PASSWORD` (digest,
  single-use nonces); this holds for every bind address, loopback included. Use both when the
  trunk allows it: an ACL alone trusts source addresses, which are spoofable over UDP, and a
  digest alone leaves the port open to scans.
- **Prompt-injected actions.** Any carrier's caller can try to talk the model into a transfer or
  another SideEffect tool. List `transfer_call` (and any payment or booking tool) in
  `agent.confirm_tools` so it runs only after the caller's next reply confirms
  (`docs/TOOLS.md`, "Confirmation gate").
- **Agent link.** Transcripts, the LLM context and tool calls cross the link: plaintext `ws://`
  to a host outside the private networks is refused (`docs/AGENT_PROTOCOL.md`).

### SIP over TLS and SRTP

**Turning it on.** All of it is in `BRISVOX_SIP_*` (`docs/CONFIG.md`):

```sh
BRISVOX_SIP_ADDR=0.0.0.0:5060
BRISVOX_SIP_PUBLIC_IP=203.0.113.10
BRISVOX_SIP_ALLOWED_SOURCES=198.51.100.0/24                 # the carrier's signaling networks
BRISVOX_SIP_TLS_ADDR=0.0.0.0:5061
BRISVOX_SIP_TLS_CERT=/etc/brisvox/sips.crt                  # PEM chain, leaf first
BRISVOX_SIP_TLS_KEY=/etc/brisvox/sips.key                   # PEM, read once at startup
BRISVOX_SIP_SRTP=required                                   # off | optional | required
```

- **SIPS.** TLS 1.2 and 1.3 only (rustls, `ring`), no client certificate. The accept loop is
  Brisvox's own (`src/sip/tls.rs`), not the stack's, because the stack's TLS listener does the
  handshake without a deadline and without a limit. Before any SIP is read: the source ACL, then
  `BRISVOX_SIP_TLS_MAX_CONNECTIONS` (1024) and `BRISVOX_SIP_TLS_MAX_PER_IP` (32), then
  `BRISVOX_SIP_TLS_HANDSHAKE_MS` (10 s). After the handshake a connection is the same thing as a
  TCP one: digest, ACL, the admission gate and the call limit apply unchanged. The Contact of a
  call that came in over TLS is `sip:brisvox@ip:5061;transport=tls` (`sips:` when the INVITE's
  Request-URI was `sips:`), and the peer's BYE and re-INVITE come back on the same connection.
  The certificate is read once; there is no hot reload (restart to rotate, a draining restart
  loses no call). **Idle timeout**: an established connection that carries no byte in
  either direction for `BRISVOX_SIP_TLS_IDLE_TIMEOUT` seconds (default 300, `0` = off) is closed
  and its slots freed (`SipStats::tls_idle_closed`); an RFC 5626 CRLF keepalive is a byte and
  keeps it open. The stack reports no activity, so the TLS endpoint talks to a loopback socket
  and a relay task (`src/sip/tls_relay.rs`) copies the bytes to the peer's socket through an
  `AsyncRead`/`AsyncWrite` adapter that notes the time of each transfer; the check runs on the
  injected clock. Cost: two more descriptors and one task per TLS connection. Keep the ACL on a
  public address all the same.
- **SRTP (SDES, RFC 4568).** `AES_CM_128_HMAC_SHA1_80` and `AES_CM_128_HMAC_SHA1_32` (RFC 3711),
  keys exchanged in the SDP. The first usable `a=crypto` line of the offer wins, the answer
  repeats its tag and suite with Brisvox's own key, each direction has its own key, and every
  call gets fresh random keys wiped on drop. The packet code (`src/sip/srtp.rs`) is Brisvox's:
  AES-128 counter mode from RustCrypto (`aes`, `ctr`), HMAC-SHA1 from `ring`, in place and
  allocation-free per packet (a few microseconds on the call's task), replay window of 128
  packets, tag compared in constant time, a forged packet changes no state. Key schedules wipe
  themselves on drop (`zeroize` features of `aes` and `ctr`), as do the master and session keys
  and the IV buffers (`ring`'s HMAC key has no wipe of its own, and the base64 text of a key in an SDP buffer is
  not wiped either: the places a key outlives its call in memory until the allocator reuses it). A crypto line whose key **lifetime** is below
  2^31 packets (`|2^20`, `|1048576`) is ignored: this endpoint neither counts packets per key nor
  re-keys by itself, so it does not pretend to honour a short lifetime. Packets that fail
  authentication are counted per call per second: more than
  `BRISVOX_SIP_SRTP_AUTH_FAIL_LIMIT` (default 50, `0` = off) ends the call with a log line and
  `SipStats::srtp_flood_hangups` (a `_32` tag is 32 bits, so a flood is a guessing attack; the
  price is that whoever can send UDP to the call's RTP port can end the call, which is a cheaper
  attack than guessing). It is pinned to the
  RFC 3711 test vectors (appendix B.2 and B.3) and cross-checked against the independent
  `webrtc-srtp` crate in both directions across the sequence-number wrap.
- **Policy.** `off`: plain RTP only; an `RTP/SAVP`-only offer gets 488. `optional` (the default
  when TLS is configured): SRTP when the offer has a usable crypto line **and** came in over TLS,
  otherwise plain RTP; with both on offer SRTP is preferred. `required`: nothing but SRTP over
  TLS, everything else is 488 (`SipStats::rejected_insecure`). **SDES keys are never taken from
  UDP or TCP signaling** (RFC 4568 section 7.1: keys need confidentiality and integrity from the
  signaling), under any policy: such an offer gets 488 and no crypto line is answered. Setting
  anything but `off` without the TLS listener is a startup error. The mode of a call is fixed at
  setup: a re-INVITE cannot turn SRTP off (488, the call stays up). **In-dialog requests stay on
  TLS**: the transport of a dialog is recorded from the real connection when the call
  is set up, never read from a `Via` (written by the sender); an in-dialog request for a TLS
  dialog that does not arrive on a TLS connection gets 488 (re-INVITE, UPDATE) or 403 (anything
  else) before the dialog sees it, with no SDP processed and no key answered
  (`SipStats::insecure_dialog_requests`). The ACK of a re-INVITE is delivered by the stack
  straight to the re-INVITE's transaction without its connection, so a message inspector strips
  the SDP from an ACK that came in off TLS on a TLS dialog. **Re-keying**: a peer key that
  changed is answered with a **fresh key of ours** and fresh sender and receiver state (a leaked
  answer exposes nothing older); the same key again (hold, resume, refresh) changes nothing; a
  suite change must come with a new peer key (a new suite with the old key is refused: the `_32`
  tag is the prefix of the `_80` tag, so a receiver started over on the same key would take every
  earlier `_80` packet cut to 4 bytes as new). The answer to an offer we made in a 200 (body-less
  re-INVITE) keeps the key we offered.
- **Outbound.** A `DialRequest.to` of `sips:...` or `...;transport=tls` is called over TLS, the
  callee's certificate verified against `BRISVOX_SIP_TLS_CA` (a PEM bundle; there are no built-in
  roots in the stack, so a public carrier's CA must be listed there). With `optional` or
  `required` a TLS call offers `RTP/SAVP` with `AES_CM_128_HMAC_SHA1_80`; an answer that is not
  SRTP with the offered tag and suite ends the call (BYE, `DialError::BadAnswer`). `required`
  refuses a non-TLS target before anything is sent. With `DialRequest::destination` (an explicit
  proxy address) and a callee named by a domain, the connection is made by Brisvox and the
  certificate is verified for the **callee's domain** (the stack would verify the proxy's IP,
  which public CAs do not certify); a live connection verified for the same name is reused, and
  the handshake follows `BRISVOX_SIP_TLS_HANDSHAKE_MS`. A callee given as an IP address is
  verified for that address. Not done: DNS SRV/NAPTR for TLS targets.
- **Logs.** `rsipstack` logs every SIP message at `info`, which on a secure call includes the SDP
  and with it the SRTP master keys. The server binary therefore holds the target `rsipstack` at
  `warn` **whatever `RUST_LOG` says** (like the TURN client crates), and `DEFAULT_FILTER` names it
  too. `BRISVOX_LOG_SIP_STACK=1` lifts that for debugging only. A host that installs its own
  subscriber must apply `brisvox_core::telemetry::sip_stack_log_allowed`. The stack also logs one
  warning at start about the TLS address entry that is not a listener it can serve (a
  placeholder that makes it write `Via: SIP/2.0/TLS` for requests to TLS callees); it is expected.
  Brisvox's own events, `Debug` output and errors never contain key material (test
  `key_material_never_reaches_brisvoxs_logs_or_debug_output`).

**Which trunks were checked** (documentation fetched 2026-10-13; nothing was tested live against a
carrier; that live run is pending):

| Peer | What its documentation says | Source |
|---|---|---|
| Twilio Elastic SIP Trunking | TLS 1.2 and above, default port 5061, origination URI `sip:host:5061;transport=tls`; SRTP by SDES (RFC 4568); origination (Twilio to us) offers only `AES_CM_128_HMAC_SHA1_80`, termination accepts `_80` and `_32`; the MKI parameter is not supported; `sips:` is treated as `sip:` over TLS (Twilio recommends `;transport=tls`); with Secure Trunking enabled unencrypted calls are rejected | https://www.twilio.com/docs/sip-trunking (Secure Trunking) |
| Telnyx | TLS 1.2 and 1.3 on port 5061 and an "SRTP" encrypted-media setting on the trunk; its default offer lists `AEAD_AES_256_GCM_8` first and `AES_CM_128_HMAC_SHA1_80` as well (fetched 2026-10-08, `docs/providers/sip.md`), so Brisvox answers the `_80` line. Not re-fetched on 2026-10-13: the Telnyx pages (a search found only third-party guides) | sip.telnyx.com as recorded in `docs/providers/sip.md`; https://docs.livekit.io/telephony/features/secure-trunking/ (LiveKit's Telnyx steps) |
| Asterisk (PJSIP) | `media_encryption=sdes` over a TLS transport; default offer `AES_CM_128_HMAC_SHA1_80`, `srtp_tag_32` selects the 32-bit tag; `media_encryption_optimistic` allows plain fallback | https://issues.asterisk.org/jira/browse/ASTERISK-26190, https://community.asterisk.org/t/sdes-support-in-asterisk-18-and-pjsip/107792 |
| FreeSWITCH | `rtp_secure_media` (`mandatory`/`optional`) with a suite list that includes `AES_CM_128_HMAC_SHA1_80` and `_32` next to AEAD and 256-bit suites; offers strongest first by default, so Brisvox takes the first line it supports (`_80`/`_32`) | https://developer.signalwire.com/freeswitch/Channel-Variables-Catalog/rtp_secure_media_16353121 |
| Vonage | not checked | |
| RFC 4568 (SDES), RFC 3711 (SRTP) | `a=crypto:<tag> <suite> inline:<base64 key||salt>[|lifetime][|MKI:len]`; key||salt is 30 bytes for both suites; the answer accepts exactly one line; keys need confidential signaling | https://www.rfc-editor.org/rfc/rfc4568, https://www.rfc-editor.org/rfc/rfc3711 |

Facts about the stack itself (`rsipstack` 0.7.3 source, read 2026-10-13): its TLS listener has no
handshake deadline or connection limit (hence Brisvox's accept loop); `TlsConnection::from_server_stream`
takes a finished stream; the connection type of an outbound request comes from the URI's
`transport` parameter, not from the `sips:` scheme (Brisvox adds `;transport=tls` to a `sips:`
callee); a request's `Via` transport is chosen from the stack's listener addresses; dropping any
`TransportLayer` clone cancels the whole stack (so connection tasks never hold one); it keeps no
trust roots of its own (empty store plus the PEM it is given).

**What remains.**
- **DTLS-SRTP** (RFC 5763/5764, `a=fingerprint`, `a=setup`) is not implemented; it is what WebRTC
  peers use, and trunks mostly do not. Not cheap: a DTLS handshake per call plus SRTP key export
  (whether str0m's DTLS can be reused for it was not evaluated).
- AEAD suites (`AEAD_AES_128_GCM`/`256_GCM`, RFC 7714) and the 256-bit CM suites; SRTCP and RTCP
  (none is sent, the peer's is ignored); MKI; SDES session parameters (`KDR`, `WSH`, `FEC_*`,
  `UNENCRYPTED_*`) — lines with them are ignored, never half-honoured; optimistic SRTP
  (`RTP/AVP` with `a=crypto`) is read as plain RTP.
- Mutual TLS (a client certificate from the carrier), certificate hot reload, outbound DNS
  SRV/NAPTR, a built-in public trust store for outbound TLS. A TLS connection verified for one
  callee domain and reused for another domain through the same proxy address stays verified for
  the first name only (the stack caches connections by address).
- Interop against a real carrier or SBC over TLS+SRTP: the tests use a scripted
  peer that speaks the same RFCs as the implementation, so a shared misreading would not show.
