# Brisvox agent protocol, version 1

Status: version 1, extended with reconnecting, routing and transfer through the carrier (every
addition is optional and marked "optional extension"; the SDKs speak reconnecting too: an agent written against the first version 1
keeps working unchanged). Code: `crates/brisvox-agent-link` (`protocol.rs` is the executable definition
of every message, `link.rs` the server side, `tool.rs` the remote tools, `client.rs` the
reference agent in Rust). Engine inputs it uses: `brisvox_engine::agent_input` (bot lines,
conversation edits) and `brisvox_engine::events`.

The conversation of a call runs in brisvox (audio, VAD, STT, turn taking, LLM, TTS, barge-ins,
the tool engine). The **agent process** holds the application logic: it watches the call,
speaks lines, edits the conversation, transfers or ends the call, and implements tools. It can
be written in any language (Python SDK: `sdk/python`, see its README; TypeScript SDK: `sdk/typescript`) and runs out of the
server's process, on the same host or elsewhere.

## Transport and connection

- **The server connects out** to the agent: one WebSocket connection per call to
  `BRISVOX_AGENT_URL` (`ws://` or `wss://`; TLS with the server's built-in webpki roots, plus the
  private CA of `BRISVOX_AGENT_TLS_CA` if set). With `BRISVOX_AGENT_TOKEN` set, the handshake
  carries `Authorization: Bearer <token>`. The agent is the WebSocket server; it accepts many
  connections at once (one per live call).
- **The link carries transcripts, the LLM context and tool calls, so plaintext is refused off
  the private network**. A `ws://` URL is accepted only when its host is a loopback,
  RFC 1918 (`10/8`, `172.16/12`, `192.168/16`) or link-local (`169.254/16`) IPv4 address, an IPv6
  `::1`, unique-local (`fc00::/7`) or link-local (`fe80::/10`) address (an IPv4-mapped one counts
  by its IPv4 part; an IPv6 zone id such as `fe80::1%25eth0` counts only on a link-local address,
  so a public address with a zone is still public), `localhost` or a name under `.localhost`,
  `.internal` (Railway, AWS and GCP private DNS) or `.local` (mDNS, Kubernetes
  `*.cluster.local`), or a single-label name without a dot (a Docker Compose or Kubernetes service
  name such as `agent`). A single-label name is accepted but logged once as a warning: the
  resolver's search list can turn `agent` into `agent.example.com`, so prefer the full
  `.internal`/`.local` name. The rule reads the URL text only; it does not resolve names. The
  server logs and reports the agent URL as `scheme://host:port` only (no userinfo, path or query). Any other host with `ws://` fails the configuration at
  startup; use `wss://`, or set `BRISVOX_AGENT_ALLOW_INSECURE=1` (`[agent_link] allow_insecure =
  true`, or `allow_insecure = true` on one route) when another layer protects the path (a VPN, a
  service mesh). Allowing it logs a warning at startup. `wss://` to any host is always accepted.
- **Private CA.** `BRISVOX_AGENT_TLS_CA=/path/ca.pem` (a PEM bundle, at most 16 certificates and
  256 KiB, read at startup) adds those certificates to the webpki roots for every `wss://` agent
  URL of the server, so an internal agent behind a private CA verifies like a public one. A
  missing, empty or malformed file fails the startup. SPKI pinning is not implemented.
- Every message is **one text frame holding one JSON object** with a `type` field. Binary frames
  are not part of the protocol (answered with an `error`). Largest message: **64 KiB** either
  way; the server closes a link that sends a larger one.
- Unknown fields are ignored by both sides (additions within version 1 stay compatible). An
  unknown `type` from the agent is answered with `unknown_type`.
- The server sends a WebSocket ping every 10 s; standard WebSocket stacks answer it by
  themselves. A link from which nothing (no message, no pong) arrived for 30 s is considered
  down.

## Version negotiation and handshake

1. The server connects (bound: `connect_timeout`, 2 s) and sends `session.start`, offering the
   versions it speaks in `versions` (`[1]` in this build).
2. The agent answers within `handshake_timeout` (2 s, counted from the connection: the
   `session.start` write is part of it) with **either**
   - `session.accept` with the `version` it picked from `versions`, its tools, and optionally a
     system prompt, an initial context, a greeting and an event subscription; **or**
   - `session.reject` (`code`, `message`): the agent does not take this call. An agent that
     speaks none of the offered versions rejects with code `unsupported_version`.
3. A `version` not in `versions`, any other first message, a late answer or a closed
   connection fail the handshake: the server logs it and the call runs **without** the agent
   (see "Failure and fallback").

The call's pipeline starts only after the handshake, so the agent's prompt, tools and greeting
apply from the first word. The caller hears silence meanwhile: at most `connect_timeout +
handshake_timeout` (4 s with the defaults) when the agent URL is unreachable, black-holed or
slow, after which the call starts on the agent definition alone. `handshake_timeout` bounds the
`session.start` write and the answer together, so no other bound adds to it.

```json
{"seq":1,"t_ms":0,"type":"session.start","protocol":"brisvox.agent","versions":[1],
 "session_id":"42","agent":"luigi","capabilities":{"transfer":true,"tools":true}}

{"type":"session.accept","version":1,
 "system_prompt":"You are Luigi's host.",
 "context":[{"role":"system","text":"The caller is a regular."}],
 "greeting":"Hi, Luigi's here. How can I help?",
 "events":["user.text","turn.bot_ended"],
 "tools":[{"name":"book_table","description":"Books a table.",
           "parameters":{"type":"object","properties":{"time":{"type":"string"}},"required":["time"]},
           "kind":"side_effect","timeout_ms":4000}]}
```

`session.accept` fields:

| field | type | meaning |
|---|---|---|
| `version` | integer | The version picked. Required. |
| `tools` | array of tool declarations | Remote tools (below). At most 64. |
| `system_prompt` | string | Replaces the agent definition's prompt (at most 64 KiB). |
| `context` | array of `{role, text}` | Messages the conversation starts with (at most 32; roles `system`, `user`, `assistant`). |
| `greeting` | string | A line the bot speaks at once (a bot line, below). |
| `resume` | boolean | Optional extension (reconnecting). `true`: the agent keeps the call's state after a lost connection and can take the call up again; the server then reconnects (see "Reconnecting"). Absent or `false`: the link is not reopened. |
| `events` | array of event types | Only these events are sent (absent: all). `session.*`, `tool.*`, `command.result`, `events.lost` and `error` are always sent. |

`session.start.capabilities.transfer` is `true` when the call's transport has a carrier binding
(Twilio or Telnyx call control, `docs/TELEPHONY.md`) that can transfer it: `transfer` then goes
through the same binding as the `transfer_call` tool, whether or not the agent definition names
that tool. It is `false` for calls no carrier owns (generic websocket, RTVI, SIP) and in a build
without the `call-control` feature.

`session.start` may also carry `call`, who is on the line, when the agent definition opted in
(see "Caller identity" below); it is absent otherwise.

Problems in an accepted handshake that do not prevent the call (a refused tool, an unknown event
type, an empty context message) are reported as `error` events with `stage: "agent_link"`.

## Server to agent

Every server message has an envelope: **`seq`** (1 for `session.start`, then one more for every
message written on the link) and **`t_ms`** (milliseconds since the link opened, on the call's
clock). The fields below are the body.

### Events

| `type` | fields | meaning |
|---|---|---|
| `turn.user_started` | `epoch` | A user turn started (speech; also a confirmed barge-in). |
| `turn.user_ended` | `epoch` | The user turn ended; the engine answers it now. |
| `user.transcript` | `text`, `final` | A transcript segment from the STT (interim when `final` is false). |
| `user.text` | `epoch`, `text` | The whole text of the user turn the LLM answers (spoken finals and typed text). |
| `bot.llm_started` | `epoch` | The LLM started a response round (a tool loop has several). |
| `bot.text` | `epoch`, `text` | A streamed LLM delta. A bot line arrives as one delta. |
| `bot.llm_stopped` | `epoch` | The round ended. |
| `bot.tts_text` | `epoch`, `text` | A chunk sent to the TTS. |
| `bot.tts_stopped` | `epoch` | The TTS produced the response's last audio. |
| `turn.bot_started` | `epoch` | The bot's audio of `epoch` started playing. |
| `turn.bot_ended` | `epoch` | The bot's audio of `epoch` was played to its end. |
| `turn.interrupted` | `epoch` | A barge-in: the call moved to `epoch`; everything older was dropped. |
| `turn.metrics` | `epoch`, `metrics` (`end_of_turn_ms`, `stt_final_ms`, `llm_ttft_ms`, `tts_ttfb_ms`, `response_ms`; absent when unknown) | The turn's latencies when its first audio went out. |
| `error` | `stage`, `message`, `fatal` | A stage of the call reported an error, or the link refused something without a command id (`stage: "agent_link"`, the message starts with the error code). |
| `events.lost` | `count` | Events the agent did not get (see "Backpressure"). |
| `session.end` | `reason` (`call_ended`, `hangup`, `ended_by_agent`, ...) | The last message; the server closes the link after it. |

The epochs are the engine's turn numbers: a bot event of an epoch older than the newest
`turn.interrupted` or `turn.user_started` belongs to a response that was cut.

### Tool calls

| `type` | fields | meaning |
|---|---|---|
| `tool.call` | `id` (integer), `call_id`, `name`, `arguments` (object, valid against the declared schema), `kind`, `epoch`, `idempotency_key` (16 hex digits), `timeout_ms` | Run the tool and answer with `tool.result` echoing `id`. |
| `tool.cancel` | `id`, `reason` (`interrupted`, `timeout`, `abandoned`) | The server stopped waiting: a later `tool.result` is ignored. Advice only for a `side_effect` tool. |
| `command.result` | `id`, `ok`, `error` (`{code, message}`, when not ok) | The outcome of a command that carried an `id`. |

## Agent to server

### Commands

Every command may carry **`id`** (string, at most 64 bytes; echoed in `command.result`, which is
sent only when `id` is present) and **`re`**: the `seq` of the event the command reacts to (the
server measures the hop latency with it, see `docs/LATENCY.md`).

| `type` | fields | meaning |
|---|---|---|
| `say` | `text` (at most 4 KiB), `interrupt` (default false) | A **bot line**: the bot speaks `text` verbatim, without an LLM request. |
| `end` | `say` (optional farewell), `reason` | End the call: speak the farewell (a plain line), wait until the bot is silent, then end the call. Bounded by `end_grace` (30 s). `command.result` comes when the call is being ended. |
| `transfer` | `destination` (E.164: `+` and 8 to 15 digits), `mode` (`cold` default, `warm`), `summary`, `reason` (each at most 512 bytes) | Transfer through the carrier's call-control (Twilio/Telnyx REST, `docs/TELEPHONY.md`). Waits for the bot to finish its sentence first. `unsupported` without a call-control (`session.start.capabilities.transfer`), and for `warm` when warm transfers are not on for the agent (`[telephony] warm`, the callback listener and a binding that can do them: the same gate as the tool). After a successful `cold` transfer the caller is on the destination's line and the bot leaves the call (the link ends with `session.end`). The carrier's idempotency key is derived from the call and the command `id`: a repeated `transfer` with the same `id` (after a lost result or `outcome_unknown`) carries the same key, so a carrier that deduplicates (Telnyx `command_id`) ignores it. **Always give a `transfer` an `id`**: without one, each command is a new transfer. |
| `context.update` | `system_prompt`, `append` (at most 32 `{role, text}`) | Edit the conversation (all of the command or nothing). |
| `tools.set` | `enabled`: array of tool names, or `null` | Offer only these tools (local and remote) to the LLM; `null`: all again. |
| `tool.result` | `id` (the `tool.call`'s), `result` (any JSON) or `error` (string) | The answer of a remote tool. A string `result` reaches the LLM as it is; any other JSON in compact form; `error` as a `tool_failed` error result. |

### How the commands act on the call

They go through the engine's own inputs; none of them touches the pipeline directly.

- **Bot lines** (`say`, `end.say`, the greeting) go to the turn driver, the epoch authority. A
  line is a *bot turn*: when nobody speaks and no response runs, the driver starts a new epoch
  and the LLM runner speaks the text as that epoch's response (`bot.llm_started`, one
  `bot.text`, `turn.bot_started` ... `turn.bot_ended`). It enters the context as an assistant
  message and is cut to what was heard on a barge-in, like any response; the caller can barge
  in on it. A line **waits** (in order, at most 8) while the user speaks or a response runs.
  With `interrupt: true` the line drops the lines waiting before it and **cuts a running
  response** (never the user: while the user speaks every line waits) like a barge-in.
- **Conversation edits** (`context.update`, `tools.set`, the initial context) are applied at
  the LLM runner's next safe point: right before the next response starts (a user turn's end, a
  bot line, a speculative start), never while a response runs, so tool calls always stay paired
  with their results. An edit made while the bot speaks shapes the next request. With a flow
  (`docs/ARCHITECTURE.md`, flows) the system prompt is the flow node's and the next transition
  replaces it again; `tools.set` then narrows the node's tools. A tool call to a tool that is
  not offered is answered `unknown_tool` without running.
- **`end`**: the server stops the call gracefully (as at a drain), then sends `session.end` with
  reason `ended_by_agent`.

## Remote tools

A tool declaration (`session.accept.tools[]`):

| field | type | meaning |
|---|---|---|
| `name` | string | 1 to 64 of `A-Z a-z 0-9 _ -`; must not collide with a tool of the agent definition. |
| `description` | string | For the LLM (at most 4 KiB kept). |
| `parameters` | JSON Schema | `"type": "object"`; compiled at the handshake (no remote `$ref`). |
| `kind` | `read_only` / `side_effect` | TOOLS.md items 7 and 8. |
| `timeout_ms` | integer | The engine's wait for the result: default 5000, at most 30 000. |
| `filler_after_ms` | integer | A filler phrase after this long without a result: default 700; 0 disables. |
| `confirm` | boolean | `side_effect` only, default `false`. Hold the tool for the user's confirmation (below, and `docs/TOOLS.md`, "Confirmation gate"). A `confirm` on a `read_only` tool refuses that declaration. |
| `repeatable` | boolean | `side_effect` only, default `false`. By default the engine refuses an identical call (same tool, same arguments) from a later response of the same turn once the first one succeeded, answering `already_done` without sending a `tool.call`: a model that read the result and asks again is repeating itself (a second booking). `repeatable: true` lets such a repeat through, for a tool where doing the same thing twice is legitimate, such as pressing the same IVR digit twice (`docs/TOOLS.md`, "What "never twice" covers"). Two identical calls in one response, a new turn and a retry of a failed call always run. A `repeatable` on a `read_only` tool refuses that declaration, like `confirm`: a read-only tool may always repeat, so the flag would mean nothing. An agent written before the field existed declares no `repeatable` and keeps the default. |

A remote tool is an ordinary tool of the call's tool engine (`docs/TOOLS.md`) whose execution is
an RPC: the engine validates the arguments, applies the retry budget, the idempotency store, the
filler policy and the result bounds, then the link sends `tool.call` and waits for `tool.result`.

- **`read_only`**: the engine stops waiting at `timeout_ms` or when the caller barges in; the
  link then sends `tool.cancel` (`abandoned` or `interrupted`) and ignores a late result. The
  LLM gets a `timeout` / `interrupted` error result. May run during a speculative response.
- **`side_effect`**: never speculative, never run twice for one call id (`idempotency_key`
  is stable for that tool call, not across phone calls or turns: pass it to APIs that deduplicate
  a redelivery, and use a key of your own, such as the order id, for "once ever" rules;
  `docs/TOOLS.md`, "What "never twice" covers"). At `timeout_ms` the engine answers
  the LLM with a `timeout` error but the call keeps waiting: the link waits up to
  `side_effect_wait` (60 s, under the engine's 120 s backstop) and a result that arrives then
  reaches the conversation as a late note. After that wait, or when the link goes down,
  the outcome is recorded as unknown (`the outcome is unknown`), never as a failure, and the
  call is not repeated.
- At most 32 calls wait at once per link; more fail at once (`tool_failed`).
- **`confirm: true`** (a prompt-injection guard): when the model calls the tool, the server does
  **not** send `tool.call`. It records a pending confirmation and gives the model a
  `confirmation_required` result; the tool is offered to the agent as a `tool.call` only when the
  model calls the built-in `confirm_pending` tool (added to the model's tool list automatically)
  after the user's *next* reply. The agent sees one `tool.call`, with the arguments of the
  original request, and never sees a held request that was not confirmed. The rules (a later user
  turn, expiry, barge-in clears, once only) are in `docs/TOOLS.md`.

## Ordering

- The server writes messages in one task: `seq` order is write order. Events keep the engine's
  order (the order the stages handled them, which follows the pipeline's causal order).
  Control messages (`tool.call`, `tool.cancel`, `command.result`, `error`) are written before
  queued events, so a tool call is never stuck behind a burst of text deltas.
- The agent's messages are applied in arrival order by one task; `command.result`s come back in
  that order, except for `end` and `transfer`, which run on a separate task (one at a time) and
  answer when done.
- A `tool.result` may come at any time, in any order.

## Backpressure and bounds

| queue | bound | when full |
|---|---|---|
| events to the agent | 256 | the newest event is dropped and counted; the next event that fits is preceded by `events.lost` with the count |
| engine event ring | 1024 (shared with the RTVI bridge) | the oldest are lost, reported the same way |
| control messages to the agent | 64 | a tool call fails at once; a `command.result` is dropped (counted) |
| bot lines into the call | 16 queued + 8 held by the turn driver | `queue_full` |
| conversation edits into the call | 64 | `queue_full` (nothing of the command is applied) |
| `end` / `transfer` | 4 | `queue_full` |
| remote calls waiting | 32 | `tool_failed` at once |

The server never waits on the agent: every write is bounded by `write_timeout` (2 s; a slower
write takes the link down) and every command is queued without waiting. A slow agent loses
events, never stalls the call. Use `session.accept.events` to receive only what you need (for
example without `bot.text` and `bot.tts_text` deltas).

## Error codes

| code | meaning |
|---|---|
| `bad_message` | Not JSON, a missing field, a field of the wrong type. |
| `unknown_type` | A `type` this version does not know. |
| `invalid_argument` | A value breaks a rule (an empty or too long text, a bad phone number). |
| `too_large` | A count bound was hit (messages of one `context.update`, tool names). |
| `queue_full` | The call's input queue is full: retry later. |
| `unsupported` | The call cannot do this (no call-control for `transfer`). |
| `call_ended` | The call is over. |
| `not_ready` | A handshake message after the handshake. |
| `rejected` | The carrier refused the transfer; nothing changed. |
| `unavailable` | The carrier could not be reached; nothing changed. |
| `outcome_unknown` | The transfer may or may not have started (a timeout): do not repeat it blindly. |
| `unsupported_version` | For `session.reject`: the agent speaks none of the offered versions. |
| `unknown_session` | For `session.reject` of a resumption: the agent does not know the call (it restarted, or the parked call expired). |

## Reconnecting

Optional extension. An agent that declares `"resume": true` in `session.accept` promises to keep the
call's state (keyed by `session_id`) when its connection is lost, for a while, and to take the
call up again on a new connection. For such an agent a lost link (the agent or the network
closes the socket, a write times out, 30 s of silence) is reopened instead of being given up:

1. The call goes on at once as in "Failure and fallback": remote tool calls waiting end
   (`read_only`: `tool_failed`; `side_effect`: outcome unknown, never repeated), the LLM is
   offered the agent definition's tools only, events are not delivered, commands cannot arrive.
2. The server waits (backoff: doubling from 100 ms up to 2 s, `AgentLinkConfig::reconnect_backoff`,
   scaled by a stable random factor of 0.5 to 1 so the calls of a crashed agent do not return
   in the same instant) and opens a new connection to the same URL, with the same token.
3. It sends `session.start` again, carrying `resume`:

```json
{"seq":57,"t_ms":41200,"type":"session.start","protocol":"brisvox.agent","versions":[1],
 "session_id":"42","agent":"luigi","capabilities":{"transfer":true,"tools":true},
 "resume":{"attempt":1,"previous_seq":55,"down_ms":120}}
```

   `seq` and `t_ms` continue the call's count (`previous_seq` is the last message written on
   the lost connection), so a `seq` is never repeated within a call. `attempt` counts the
   attempts of this outage; `down_ms` is the time since the loss.
4. The agent answers within the handshake timeout with `session.accept` (`version`; optionally
   `events` to re-select the subscription, and `"resume": true`, required) or `session.reject`.
   An accept of a resumption without `"resume": true` (an agent that restarted and kept no
   state) ends reconnecting like a reject. In the accept of a resumption
   `tools`, `system_prompt`, `context` and `greeting` are ignored: the call keeps what it has
   (the remote tools declared first stay registered; a restarted agent must declare the same
   tools). A `session.reject` (use the code `unknown_session` for an agent that does not know
   the call), an accept without `resume: true` or a protocol error ends reconnecting: the call goes on without the agent.
5. On success the link is up again: the remote tools are offered to the LLM again (a
   `tools.set` made before the loss is not restored: send it again), events flow, and the
   first event is preceded by `events.lost` with the number of events missed meanwhile. Ids of
   `tool.call` never repeat within a call, so a late `tool.result` for a call that was failed
   at the loss is simply ignored.

Bounds: at most `BRISVOX_AGENT_RECONNECT_ATTEMPTS` attempts (default 5; `0` turns reconnecting
off) in one series, no new attempt more than `BRISVOX_AGENT_RECONNECT_WINDOW_MS` (default 15 s)
after the loss, each attempt bounded by the connect and handshake timeouts. A connection that
lived 5 s starts a new series. When the attempts or the window run out, the fallback below is
final for the call. An `end` command stops reconnecting at once (an agent leaves right after
`end`; the farewell is still spoken). What the server had queued for the lost connection
(tool calls, command results, events) is dropped, never delivered late. Commands that were in
flight at the loss may or may not have been applied; give a `transfer` an `id` and repeat it
with the same `id` if its result was lost (the carrier key is stable per `id`).

**In the SDKs** (`sdk/python` `Agent(resume=True)`, `sdk/typescript`
`new Agent({ resume: true })`, both opt-in and off by default): the SDK declares `resume` in
`session.accept` and, when a link is lost, *parks* the call's session instead of ending it. The
session keeps its `data`, its queued events and its place in the handlers' order; commands fail
with the SDK's closed-session error while it is parked, and running tools hear `link_closed`
as before. A `session.start` with `resume` for a parked `session_id` is answered with
`session.accept` (`version`, the first accept's `events`, `"resume": true`; no `tools`, prompt,
context or greeting) and the session moves onto the new link: no second `on_session_start` /
`onSessionStart`, no greeting, `seq` continuing. The handlers then see an SDK-made
`session.resumed` event (`on_resumed` / `onResumed`: `attempt`, `previous_seq`, `seen_seq` (the last `seq` the SDK itself read on the lost link), `down_ms`; it is
not on the wire and not subscribable), after the events queued before the loss, followed by the
server's `events.lost`. A parked session lives for the *resume window* (default 15 s, the
server's own `BRISVOX_AGENT_RECONNECT_WINDOW_MS`); then the handlers get one `session.end`
(`link_closed`), the session is forgotten and a later resumption is answered with
`session.reject` (`unknown_session`). A `session.end` from the server is final at once and is
never parked. A `session.start` with `resume` for a call whose old link the agent still holds
(half-open TCP) takes the session over and closes the old link.

## Routing

Optional extension. The agent process of a call is picked in this order:

1. The **route** the call's start message names: the metadata key `agent_route` of a generic
   websocket `start` message, the TwiML `<Parameter name="agent_route" .../>` of a Twilio
   stream. The name must be one of the definition's `[[agent_link.routes]]` (`docs/CONFIG.md`);
   a start message never carries a URL, so a caller cannot make the server connect to an
   address of their choosing. An unknown route falls back to the next rule (and is logged).
2. The definition's `[agent_link] url`.
3. The server-wide `BRISVOX_AGENT_URL`.

Each route has its own `token_env`; the default token (`BRISVOX_AGENT_TOKEN`, or the
definition's `token_env`) is only sent to the default process. With routes configured, a call
waits at most 1 s for its start message before it links (a carrier sends it at once).

## Caller identity

An addition within version 1, **off by default**. A telephony call can tell the agent process who
is on the line: `session.start` then carries a `call` object (and so does the `session.start` of
a resumption). Without the opt-in the message is byte for byte what it was before.

```json
{"seq":1,"t_ms":0,"type":"session.start","protocol":"brisvox.agent","versions":[1],
 "session_id":"42","agent":"luigi","capabilities":{"transfer":true,"tools":true},
 "call":{"carrier":"twilio","carrier_call_id":"CA0123456789abcdef0123456789abcdef",
         "from":"+15551230001","to":"+15551230002"}}
```

| field | type | meaning |
|---|---|---|
| `carrier` | string | `twilio`, `telnyx` or `sip`. |
| `carrier_call_id` | string | The carrier's id of the call: the Twilio `CallSid`, the Telnyx `call_control_id`, the SIP `Call-ID`. It is the id the carrier's own API and your carrier logs use. |
| `from` | string | The caller's number. Absent when the carrier did not say, and when the numbers are withheld (below). |
| `to` | string | The called number, with the same absence rules. |
| `pii_redacted` | boolean | Present and `true` when `from` and `to` were withheld because the server redacts phone numbers. Absent means `false`. |

Where each value comes from:

- **Twilio.** The media stream's `start` message carries the `CallSid` but no numbers. `from` and
  `to` are the stream parameters named `from` and `to` (`From` and `To` also work), so the TwiML
  that opens the stream passes them: `<Parameter name="from" value="..."/>` and `<Parameter
  name="to" value="..."/>`, with the values taken from the voice webhook's `From` and `To`
  (`docs/TELEPHONY.md`). Without those parameters only `carrier_call_id` is sent.
- **Telnyx.** The stream's `start` message carries `from`, `to` and the `call_control_id`.
- **SIP.** The user parts of the INVITE's `From` and `To` headers (`+15551230001` in
  `"Ann" <sip:+15551230001@trunk.example>`; a `tel:` URI works too; a URI without a user part
  gives no number) and the `Call-ID`. On an outbound call `from` is the number the call is
  placed from and `to` the callee. **The SIP headers are the caller's own, unauthenticated
  text**, and the user part of a SIP URI is free text. The server therefore passes a user part
  on only when it is made of `0-9 A-Z a-z + * # . _ -` (at most 64 characters) and drops the
  field otherwise. Even so it is data from outside: an agent must never put `from` or `to`
  unquoted into an LLM prompt (a user part such as `ann` is a name the caller chose, not a fact),
  and should treat them as an untrusted lookup key.
- **No carrier id, no identity.** `carrier_call_id` is always the carrier's own id. When the
  carrier's start message did not give one (a Twilio `start` without `CallSid`, say), `call` is
  absent; the server does not substitute its own stream or session id.
- The generic websocket, RTVI and WebRTC have no carrier: `call` is absent even when the
  definition opted in.

The opt-in is the definition's `[agent_link] caller_identity = true` (`docs/CONFIG.md`), or
`BRISVOX_AGENT_CALLER_IDENTITY=1` for the default agent process of the server; the file wins when
both are set. A named route has its own `caller_identity` and never inherits the section's or the
environment's, since a route may be another team's agent. With the opt-in on, a call waits up to
1 s for its carrier start message before it links (as with routes); a carrier sends it at once.
This includes a generic websocket call when the default process is opted in: the wait is for the
client's `start` message (`call` stays absent, since a websocket has no carrier), so a client
that connects and stays silent is linked after 1 s with no identity, not at once.

The numbers are personal data (`docs/PRIVACY.md`). They are withheld, and `pii_redacted` says so,
while the server redacts phone numbers anywhere: recording with redaction on and the `phone` kind
(or the `number` kind, for long digit runs, which counts as covering phone numbers) selected (the default whenever
`BRISVOX_RECORDING_DIR` is set), or `BRISVOX_LOG_REDACT_PII=true` with the `phone` or `number`
kind. `carrier` and `carrier_call_id` are still sent. Keep the numbers out of
your own logs, and remember that a caller ID is what the carrier reports: it can be spoofed, so
it is a hint for the caller, not a proof of identity.

## Failure and fallback

**The agent link never fails the call.** The configured local behaviour is the agent definition
(`docs/CONFIG.md`): its prompt, its local tools, its providers.

- Connection refused, handshake timeout, `session.reject`, an unsupported version, a bad first
  message: the call runs on the agent definition alone (logged as a warning).
- The link fails during the call (the agent closes or crashes, a write times out, 30 s of
  silence) and the agent did not declare `resume` (or the attempts are used up, see
  "Reconnecting"): the call goes on. Waiting remote calls end at once (`read_only`: `tool_failed`;
  `side_effect`: outcome unknown); events and commands stop. The call goes back to its local
  tools: from the next response on, the LLM is offered the agent definition's tools only (the
  remote tools, which would all fail now, and any `tools.set` of the agent are undone). The
  agent's system prompt and the conversation edits already applied stay: they are the
  conversation the caller had. The LLM keeps answering with that context. The link is not
  reopened (unless the agent declared `resume`).
- An `end` received before the failure still completes: the farewell is spoken and the call
  ends when it was played (the agent may close its socket right after `end`).
- The call ends (hang-up, drain, shutdown): the server sends `session.end` and closes the link
  (bounded by `close_timeout`, 1 s).

## Server configuration

| variable | default | meaning |
|---|---|---|
| `BRISVOX_AGENT_URL` | unset (no agent) | The agent's `ws://` / `wss://` URL; every call links to it. |
| `BRISVOX_AGENT_TOKEN` | unset | Sent as `Authorization: Bearer <token>`. Never logged. |
| `BRISVOX_AGENT_ALLOW_INSECURE` | off | `1` accepts `ws://` to a host outside the private networks (see "Transport and connection"). Logs a warning. |
| `BRISVOX_AGENT_CALLER_IDENTITY` | off | `1` sends the caller identity of telephony calls to the default agent process (see "Caller identity"). The definition's `caller_identity` wins; routes never inherit it. |
| `BRISVOX_AGENT_TLS_CA` | unset | PEM bundle of a private CA, trusted besides the webpki roots for `wss://` agent URLs. |
| `BRISVOX_AGENT_CONNECT_TIMEOUT_MS` | 2000 | Bound on the connection and WebSocket handshake. |
| `BRISVOX_AGENT_HANDSHAKE_TIMEOUT_MS` | 2000 | Bound on the agent's `session.accept`. |
| `BRISVOX_AGENT_RECONNECT_ATTEMPTS` | 5 | Reconnection attempts per series for an agent that declared `resume` (`0`: never reconnect). |
| `BRISVOX_AGENT_RECONNECT_WINDOW_MS` | 15000 | No new attempt starts later than this after the loss. |

The definition's `[agent_link]` section (`url`, `token_env`, `allow_insecure`, `caller_identity`, `[[routes]]`, `docs/CONFIG.md`)
refines the URL and token per agent definition and per route (see "Routing").

The other bounds (`AgentLinkConfig`: queues, keepalive, write and close timeouts, tool timeout
bounds, `side_effect_wait`, `ops_timeout`, `end_grace`) are library settings.

## A minimal agent

The reference agent (`brisvox_agent_link::client`) in a few lines: accept the session, answer
every user turn with a line, and implement one tool.

```rust
let server = AgentServer::bind("127.0.0.1:8765".parse()?, SystemClock::shared()).await?;
let mut session = server.accept().await?;
session.accept_session(SessionAccept { version: 1, tools: vec![lookup()], ..Default::default() }).await?;
while let Some(message) = session.recv().await? {
    match message.body {
        ServerBody::UserText { text, .. } if text.contains("hours") => {
            session.send(&AgentMessage::Say { id: None, re: Some(message.seq),
                text: "We open at noon.".into(), interrupt: false }).await?;
        }
        ServerBody::ToolCall { id, arguments, .. } => {
            session.send(&AgentMessage::ToolResult { id, result: Some(lookup_run(arguments)), error: None }).await?;
        }
        _ => {}
    }
}
```

## Known limits (follow-ups)

- A resumed link does not replay what the agent missed (only `events.lost` says how much); an
  agent that needs the conversation after a restart reads it from its own store or asks for it
  with `context.update`.
- The SDKs keep a parked call's state in the agent's own process memory: an agent
  that restarts answers a resumption with `unknown_session`, and a load balancer in front of
  several agent processes must route a resumption to the process that holds the call. The Rust
  reference agent (`brisvox_agent_link::client`) shows the handshake.
- `transfer` needs a call-control binding on the call (`TelephonyOps`, such as
  `brisvox_transport::call_control::BoundCall`): the server binary passes the call's own
  binding (Twilio, Telnyx; build with `call-control`) and reports it in
  `capabilities.transfer`.
- The agent cannot cancel a running LLM response without speaking (an interrupting `say`
  always speaks); a plain "stop talking" command is a candidate for version 1 additions.
