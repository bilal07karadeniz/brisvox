# @brisvox/agent (TypeScript SDK)

The agent side of the brisvox agent protocol (`docs/AGENT_PROTOCOL.md`, version 1): write the
application logic of a real-time voice call in async TypeScript. The brisvox server connects **out**
to your agent over a WebSocket, one connection per call; this package is the WebSocket server and
the typed API on top of it. It mirrors the Python SDK (`sdk/python`) one to one.

Node 20 or newer, ESM, TypeScript `strict`. One runtime dependency: `ws` (pinned to 8.22.0, the
version the suite runs on). No schema library: tool parameters use a small builder in `src/schema.ts` (see "Tools").

```ts
import { Agent, s } from "@brisvox/agent";

const agent = new Agent({ name: "luigi", systemPrompt: "You are Luigi's host.", greeting: "Hi, Luigi's here!" });

agent.tool({
  name: "opening_hours",
  description: "Opening hours of the restaurant on a weekday.",
  parameters: s.object({ day: s.enum(["mon", "tue", "wed", "thu", "fri", "sat", "sun"]) }),
  handler: async ({ day }) => (day === "mon" ? "Closed on Monday." : "12:00 to 22:00."), // day is typed
});

agent.onUserText(async (session, event) => {
  if (event.text.toLowerCase().includes("bye")) {
    await session.end({ say: "Goodbye!" }); // speaks the farewell, then ends the call
  }
});

await agent.run({ port: 8765 }); // then: BRISVOX_AGENT_URL=ws://127.0.0.1:8765
```

Install from the repository (the package is not on npm yet):
`npm install ./sdk/typescript` after `npm run build` in it, or work in the repository directly.

**Where it listens.** `host` defaults to `127.0.0.1`: only a server on the same machine can reach
the agent. For a brisvox server in another container or service bind `0.0.0.0` (IPv4) or `::`
(IPv6; `::` on a private network that may be IPv6-only, such as Railway's), set `BRISVOX_AGENT_TOKEN`, and give the server the
agent's name in `BRISVOX_AGENT_URL`. `agent.run()` logs the address it bound at `info` (printed
to stderr when the default logger, which shows warnings only, would drop it), and on a loopback
address the line says so:
`agent 'luigi' listening on ws://127.0.0.1:8765 (loopback: reachable only from this machine; bind 0.0.0.0 or :: for a server in another container)`.

## What you get

| Protocol | SDK |
|---|---|
| Handshake, version negotiation | Automatic. The highest version both sides speak is picked; none in common sends `session.reject` with `unsupported_version`. `agent.onSessionStart(...)` may return `{ systemPrompt, context, greeting, events, tools }` for this call or throw `new Reject(code, message)`. |
| `session.accept` fields | `new Agent({ systemPrompt, context, greeting, events })`. `events: "auto"` (default) subscribes only to the event types you registered handlers for. |
| Events | `agent.onUserText`, `onTranscript`, `onBotText`, `onTurn`, `onError`, `onEventsLost`, `onResumed`, `onSessionEnd`, or any event by wire type: `agent.on("turn.interrupted", ...)`. Events are readonly objects with a `type` discriminant (so `if (event.type === "turn.metrics")` narrows), camelCase fields (`tMs`, `sessionId`). Handlers of one call run one at a time, in order. |
| Remote tools | `agent.tool({ name, description, parameters, handler, kind, timeoutMs, fillerAfterMs, confirm, repeatable })`. With `parameters: s.object({...})` the handler's `args` are typed and validated; with a plain JSON-schema object they arrive as sent. The handler's second argument is a `ToolContext` (`idempotencyKey`, `epoch`, `signal`, `session`). |
| Who is calling | `session.call` is `undefined` unless the agent definition opted in (`[agent_link] caller_identity = true`, or `BRISVOX_AGENT_CALLER_IDENTITY=1`) and the call comes through a carrier (Twilio, Telnyx, SIP). Then it is a `CallIdentity`: `carrier`, `carrierCallId` (Twilio `CallSid`, Telnyx `call_control_id`, SIP `Call-ID`), `from`, `to` and `piiRedacted` (the Python SDK calls `from` and `to` `from_number` and `to_number`). The numbers are absent when the carrier did not say or when the server redacts phone numbers (`docs/PRIVACY.md`); with Twilio they exist only when the TwiML passes them as stream parameters `from` and `to` (`docs/TELEPHONY.md`). A caller ID can be spoofed: use it as a hint, and keep it out of your logs. |
| Repeating a side effect | The server refuses an identical `side_effect` call (same arguments) that the model repeats in a later response of the same turn, answering `already_done` without calling your handler. `repeatable: true` lets such a repeat through, for a tool where doing the same thing twice is legitimate (pressing the same IVR digit twice). It needs `kind: "side_effect"` (`RangeError` on a read-only tool, like `confirm`). |
| `tool.cancel` | `read_only`: `ctx.signal` aborts and the SDK stops waiting (no result is sent). `side_effect`: `ctx.signal` aborts as advice, the tool is not stopped, its result still goes out and becomes a late note. |
| Commands | `await session.say(text, { interrupt })`, `end({ say, reason })`, `transfer(destination, { mode, summary, reason, id })`, `updateContext({ systemPrompt, append })`, `setTools(names \| null)`. Each awaits its `command.result` and throws `CommandError` (`.code` is an `ErrorCode`) on refusal; `{ wait: false }` returns `{ id, result: Promise }` instead. |
| `re` | Taken from the event the handler is reacting to (an `AsyncLocalStorage`), so the server can measure the hop latency. Pass `{ re: 12 }` to override, `{ re: null }` to send none. |
| Rules of the doc | Checked before sending (4 KiB lines, E.164 numbers, 512-byte notes, 32 messages, 64 KiB messages, id <= 64 bytes): a violation throws `CommandError(invalid_argument / too_large)` locally. |
| Auth | `agent.listen({ token })` / `BRISVOX_AGENT_TOKEN`: the `Authorization: Bearer` header is checked in constant time, else HTTP 401 before any frame. `tls: { key, cert }` serves `wss://`. |

## Tools

```ts
const params = s.object({
  time: s.string({ description: "HH:MM, 24 hours" }),
  party_size: s.optional(s.integer({ minimum: 1, maximum: 12 }), 2),   // default 2: always a number in the handler
  contact: s.optional(s.object({ name: s.string(), phone: s.optional(s.string()) })),
});
```

`s.string / number / integer / boolean / enum / array / object / nullable / optional` build the JSON
Schema that goes into `session.accept` (property names are the names the LLM sees, so use the
snake_case you want it to use), validate the `arguments` of each call (a violation is answered to
the LLM as `invalid arguments: $.contact.name: is required`) and type the handler (`Infer<typeof
params>`). Extra properties are dropped. For anything the builder cannot say, pass a hand-written
JSON-schema object as `parameters`: the server validates those, and `args` is
`Record<string, unknown>`.

**Why not zod.** A tool schema needs a handful of node kinds. zod 4 (4.6.7 on npm today) is a large
dependency whose JSON-Schema export would be a second API to pin and keep in step with the server's
validator; the builder is dependency free, emits exactly the keywords the server compiles, and is
typed by inference. A zod user can pass `z.toJSONSchema(schema)` as a raw `parameters` object.

## Things to know

* **Bind address.** `run()` and `listen()` listen on `127.0.0.1` by default, which only a brisvox
  server on the same machine can reach. When the server runs in another container or service
  (Docker Compose, Kubernetes, a PaaS), bind all interfaces: `agent.run({ host: "0.0.0.0", port: 8765 })`
  (`host: "::"` where the private network can be IPv6-only, as on Railway; dual-stack on Linux), and set
  `BRISVOX_AGENT_TOKEN` on both sides. An agent the server cannot reach is not an error for the
  call: it runs on the agent file alone, without your tools. `docs/DEPLOY.md`, "Your tools service".

* **In a container.** The SDK is not on npm, so an image builds it: the build context is a
  checkout of this repository, `npm ci && npm run build` runs on the copied `sdk/typescript`, and
  `dist` plus the production `node_modules` go into the final stage.
  `sdk/typescript/examples/Dockerfile` is the template
  (`docker build -f sdk/typescript/examples/Dockerfile -t restaurant-agent .` from the repository
  root); it binds all interfaces and runs as a non-root user. Its header says how to vendor the
  build into your own package. `docs/DEPLOY.md`, "Your tools service".

* **Errors in tools.** `throw new ToolError("no tables left")` tells the LLM exactly that message.
  Any other error tells the LLM only `tool failed: <ClassName>`; the details stay in your log, never
  on the phone line. A result that is not strict JSON (NaN, Infinity, a cycle, a BigInt) or larger than
  64 KiB is answered with an error instead of hanging the call; `undefined` is sent as `null`.
* **Blocking work.** Handlers run on the event loop. A synchronous CPU-heavy tool stalls every call of
  the process (and its keep-alive): move that work to a `worker_threads` worker.
* **Cancellation is cooperative.** JavaScript cannot stop a running promise. The SDK stops *waiting*
  for a cancelled `read_only` tool and for a `side_effect` tool that outlives the link by
  `sideEffectGraceMs` (default 30 s); watch `ctx.signal` to actually stop the work.
* **Transfers.** Give a transfer an `id` you can reproduce (`id: \`handoff-${session.id}\``). After a
  `CommandTimeout` (the outcome is unknown) repeat it **with the same id** and the carrier
  deduplicates; a transfer without an id is a new transfer every time (the SDK generates a fresh id
  when you pass none). `session.capabilities.transfer` tells whether the server can transfer.
* **Slow handlers** never delay tool results or command results (they are handled on the reader),
  but they delay later events of the same call. The per-call handler queue is bounded
  (`eventQueue: 1024`); when it overflows the newest events are dropped and the handlers get an
  `events.lost` in front of the next event after the gap, exactly like the server's own backpressure
  policy. `session.end` is never dropped.
* **Session end.** `onSessionEnd` runs once per call; if the link dropped without `session.end` the
  reason is `"link_closed"`. When the link drops every running tool sees `ctx.signal` abort (reason
  `"link_closed"`); `read_only` tools are abandoned, `side_effect` tools get `sideEffectGraceMs` to wind
  down before the SDK stops waiting.
* **Limits.** `maxSessions: 256` (more calls are answered with `session.reject` `unavailable`),
  `maxConcurrentTools: 64` per call, incoming frames at most 128 KiB, `commandTimeoutMs: 10000`,
  `writeTimeoutMs: 10000` (a write that does not finish takes the link down). The handshake handler
  must finish inside `handshakeTimeoutMs` (1.5 s; the server gives up at 2 s).
* **Logs.** Warnings and errors go to `console.error`; `BRISVOX_AGENT_LOG=debug|info|warn|error|silent`
  or `new Agent({ logger })` changes that.

## Reconnecting (`resume`)

By default a lost link ends the call for the handlers (`onSessionEnd`, reason `"link_closed"`), and
the server does not reopen it. Opt in with `new Agent({ resume: true })` when the agent can keep a
call's state across a dropped connection (the protocol's "Reconnecting", `docs/AGENT_PROTOCOL.md`):

```ts
const agent = new Agent({ resume: true, resumeWindowMs: 15_000 });

agent.onSessionStart((session) => {   // once per call, never for a resumption
  session.data["notes"] = [];
});

agent.onResumed((session, event) => { // attempt, previousSeq, downMs
  console.error(`link back after ${event.downMs} ms`);
});
```

- `session.accept` then carries `"resume": true`. When the link is lost the `Session` is *parked*:
  `session.data`, the queued events and the handlers' view of the call stay. Commands reject with
  `SessionClosed` meanwhile (`session.parked` and `session.closed` are true); commands waiting for a
  result fail at the loss, and running tools see `ctx.signal` abort (reason `"link_closed"`) as
  usual. A `side_effect` call that was in flight has an unknown outcome on the server's side and is
  never repeated.
- The server's next `session.start` carries `resume` (`attempt`, `previous_seq`, `down_ms`; its `seq`
  continues the call's count). The SDK answers `session.accept` with `resume: true` and the same
  event subscription, re-attaches the parked session to the new link and runs `onResumed` after the
  events queued before the loss. There is no greeting and no `onSessionStart`; `tools`,
  `system_prompt` and `context` are ignored by the server on a resumption, so the first declaration
  stands. `session.resumes` counts the resumptions of the call.
- What the server could not deliver meanwhile is not replayed: it reports the number as
  `events.lost` (so `onEventsLost` runs). An agent that needs the conversation reads it from its own
  store or asks for it with `updateContext`.
- If the server does not come back within `resumeWindowMs` (default 15000, the server's own
  reconnect window) the call ends for the handlers once (`onSessionEnd`, `"link_closed"`) and the
  parked state is dropped; a later resumption is answered with `session.reject` `unknown_session`.
  A `session.end` from the server is final at once. Closing the server (`AgentServer.close`) ends
  parked calls immediately.
- A resumption that arrives while the old link still looks open (half-open TCP) takes the session
  over and closes the old link.
- The state lives in this process. An agent that restarts has nothing to resume: it answers with
  `unknown_session` and the server goes on without it. Keep the state in a shared store if it must
  outlive the process (and route a resumption to the process that holds the call).

## Session tokens for browsers

The server's browser-facing listeners (WebRTC, RTVI, generic WebSocket) accept short-lived,
single-use tokens your backend signs with `BRISVOX_SESSION_SECRET` (`docs/SECURITY.md`):

```ts
import { mintSessionToken } from "@brisvox/agent";

const token = mintSessionToken(process.env.BRISVOX_SESSION_SECRET!, "brisvox-webrtc", {
  ttlSeconds: 60,
  agent: "support",
  maxSeconds: 600,
  meta: { user: "u-42" },
});
```

## Examples

* `examples/minimal.ts`: the TypeScript twin of the minimal agent in the protocol doc.
* `examples/restaurant.ts`: typed tools (enum, nested object, `side_effect` with an idempotency
  key), per-call setup, and every command. It is also what `ts_sdk.rs` runs.
* `examples/resumable.ts`: `resume: true`, `onResumed`, per-call state that survives a dropped
  link.

Build and run (the examples import `../src/index.js`; in an application import `@brisvox/agent`):

```
npm ci && npm run build
node dist/examples/restaurant.js --port 8765
```

## Tests

```
npm ci
npm test            # tsc, then node:test over dist/tests (node:test: no test framework dependency)
```

* `tests/protocol.test.ts`: message (de)serialization against the examples and tables of
  `docs/AGENT_PROTOCOL.md` (it reads the doc: error codes, event names, limits and the two JSON
  examples must match).
* `tests/tools.test.ts`: schema generation, argument validation, inferred types (a compile-time check).
* `tests/resume.test.ts`: reconnecting (parking, resumption, expiry, takeover of a half-open link).
* `tests/handshake.test.ts`, `events.test.ts`, `tools-wire.test.ts`, `examples.test.ts`: the agent over
  a real WebSocket against a fake brisvox server (`tests/fake-server.ts`) that replays the doc's
  sequences.
* **Against the real Rust link:** `crates/brisvox-agent-link/tests/ts_sdk.rs` runs a call (loopback
  transport, mock providers) linked to `dist/examples/restaurant.js` in a Node process: greeting, a
  `side_effect` and a `read_only` tool, and an agent-initiated `end`, the same script as
  `python_sdk.rs`. It runs when `BRISVOX_TS_SDK` names a Node 20+ executable and the SDK is built.
  A second test of the file cuts the link of a call in the middle (a TCP proxy drops the sockets)
  and checks that `dist/examples/resumable.js` takes the call up again: state kept, `seq`
  continuing, tools still working.
  `scripts/verify.sh` installs (`npm ci`), builds, runs the whole suite and sets the variable; CI sets
  `BRISVOX_REQUIRE_TS_SDK=1`, so there a missing Node fails the gate instead of skipping it:

  ```
  BRISVOX_TS_SDK=node cargo nextest run -p brisvox-agent-link --test ts_sdk
  ```
