# brisvox-agent (Python SDK)

The agent side of the brisvox agent protocol (`docs/AGENT_PROTOCOL.md`, version 1): write the
application logic of a real-time voice call in async Python. The brisvox server connects **out**
to your agent over a WebSocket, one connection per call; this package is the WebSocket server
and the typed API on top of it.

Python 3.11 or newer, one dependency (`websockets`, `>=15,<16`: the range the suite is run on, 15.0.1).

```python
from brisvox_agent import Agent, Session, UserText

agent = Agent("luigi", system_prompt="You are Luigi's host.", greeting="Hi, Luigi's here!")

@agent.tool                                   # JSON schema from the type hints
async def opening_hours(day: str) -> str:
    """Opening hours of the restaurant on a weekday."""
    return "12:00 to 22:00"

@agent.on_user_text
async def heard(session: Session, event: UserText) -> None:
    if "bye" in event.text.lower():
        await session.end("Goodbye!")         # speaks the farewell, then ends the call

agent.run(port=8765)                          # then: BRISVOX_AGENT_URL=ws://127.0.0.1:8765
```

Install from the repository: `pip install ./sdk/python` (or `pip install -e './sdk/python[test]'`).

**Where it listens.** `host` defaults to `127.0.0.1`: only a server on the same machine can reach
the agent. For a brisvox server in another container or service bind `0.0.0.0` (IPv4) or `::`
(IPv6; `::` on a private network that may be IPv6-only, such as Railway's), set `BRISVOX_AGENT_TOKEN`, and give the server the
agent's name in `BRISVOX_AGENT_URL`. `agent.run()` logs the address it bound at INFO, or prints
it to stderr when logging is not configured, and on a loopback address the line says so:
`agent 'luigi' listening on ws://127.0.0.1:8765 (loopback: reachable only from this machine; bind 0.0.0.0 or :: for a server in another container)`.

## What you get

| Protocol | SDK |
|---|---|
| Handshake, version negotiation | Automatic. The highest version both sides speak is picked; none in common sends `session.reject` with `unsupported_version`. `@agent.on_session_start` may return `Accept(...)` (prompt, context, greeting, events, tools for this call) or raise `Reject(code, message)`. |
| `session.accept` fields | `Agent(system_prompt=, context=, greeting=, events=)`. `events="auto"` (default) subscribes only to the event types you registered handlers for. |
| Events | `@agent.on_user_text`, `on_transcript`, `on_bot_text`, `on_turn`, `on_error`, `on_events_lost`, `on_resumed`, `on_session_end`, or any event by class: `@agent.on(TurnInterrupted)` / `@agent.on("bot.tts_text")`. Typed frozen dataclasses (`brisvox_agent.protocol`). Handlers of one call run one at a time, in order. |
| Remote tools | `@agent.tool(kind="read_only" or "side_effect", timeout_ms=, filler_after_ms=)`. Schema from type hints (`str int float bool list dict Optional Literal Enum dataclass date datetime Annotated[T, "doc"]`) and the docstring (`Args:`). Arguments arrive converted to the hinted types. Add `ctx: ToolContext` for the `idempotency_key`, `epoch`, and the cancel signal. Plain `def` tools run on a worker thread. |
| `tool.cancel` | `read_only`: the tool task is cancelled (`asyncio.CancelledError`). `side_effect`: not cancelled (advice only), `ctx.cancelled` is set; the result still goes out and becomes a late note. |
| Commands | `await session.say(text, interrupt=)`, `end(say=, reason=)`, `transfer(destination, mode=, summary=, reason=, id=)`, `update_context(system_prompt=, append=)`, `set_tools(names or None)`. Each awaits its `command.result` and raises `CommandError(code)` (an `ErrorCode`) on refusal; `wait=False` returns a future instead. |
| `re` | Taken from the event the handler is reacting to, so the server can measure the hop latency. Pass `re=` to override. |
| Rules of the doc | Checked before sending (4 KiB lines, E.164 numbers, 512-byte notes, 32 messages, 64 KiB messages, id <= 64 bytes): a violation raises `CommandError(invalid_argument / too_large)` locally. |
| Auth | `agent.serve(token=...)` / `BRISVOX_AGENT_TOKEN`: the `Authorization: Bearer` header is checked in constant time, else HTTP 401. |

## Things to know

* **Bind address.** `run()` and `serve()` listen on `127.0.0.1` by default, which only a brisvox
  server on the same machine can reach. When the server runs in another container or service
  (Docker Compose, Kubernetes, a PaaS), bind all interfaces: `agent.run(host="0.0.0.0", port=8765)`
  (`host="::"` where the private network can be IPv6-only, as on Railway; dual-stack on Linux), and set
  `BRISVOX_AGENT_TOKEN` on both sides. An agent the server cannot reach is not an error for the
  call: it runs on the agent file alone, without your tools. `docs/DEPLOY.md`, "Your tools service".

* **In a container.** The SDK is not on PyPI, so an image vendors it: the build context is a
  checkout of this repository and `pip install` runs on the copied `sdk/python`.
  `examples/booking-agent/Dockerfile` is the template
  (`docker build -f examples/booking-agent/Dockerfile -t booking-agent .` from the repository
  root); it binds all interfaces and runs as a non-root user. `docs/DEPLOY.md`, "Your tools service".

* **Errors in tools.** `raise ToolError("no tables left")` tells the LLM exactly that message.
  Any other exception tells the LLM only `tool failed: <ExceptionType>`; the details stay in your
  log, never on the phone line.
* **Transfers.** Give a transfer an `id` you can reproduce (`id=f"handoff-{session.id}"`).
  After a `CommandTimeout` (the outcome is unknown) repeat it **with the same id** and the carrier
  deduplicates; a transfer without an id is a new transfer every time (the SDK generates a fresh id
  when you pass none). `session.capabilities.transfer` tells whether the server can transfer.
* **Confirmation gate.** `@agent.tool(kind="side_effect", confirm=True)` holds the tool for the
  user's confirmation: the model's call does not run it, the model gets a `confirmation_required`
  result, asks the user, and the tool runs only when the model calls the built-in `confirm_pending`
  after the user's NEXT reply (`docs/TOOLS.md`, "Confirmation gate"). A barge-in cancels the held
  request. `confirm` needs a `side_effect` tool (`ValueError` on a read-only one); the name
  `confirm_pending` is reserved. Your handler runs once, with the original arguments, when confirmed.
* **Who is calling.** `session.call` is `None` unless the agent definition opted in
  (`[agent_link] caller_identity = true`, or `BRISVOX_AGENT_CALLER_IDENTITY=1`) and the call comes
  through a carrier (Twilio, Telnyx, SIP). Then it is a `CallIdentity` with `carrier`,
  `carrier_call_id` (Twilio `CallSid`, Telnyx `call_control_id`, SIP `Call-ID`), `from_number`,
  `to_number` and `pii_redacted` (the wire fields are `from` and `to`; the TypeScript SDK names
  them `from` and `to` too). The numbers are `None` when the carrier did not say or when
  the server redacts phone numbers (`docs/PRIVACY.md`); with Twilio they exist only when the
  TwiML passes them as stream parameters `from` and `to` (`docs/TELEPHONY.md`). A caller ID can
  be spoofed: use it as a hint, and keep it out of your logs (`repr()` of a `CallIdentity` omits
  the numbers). A malformed `call` object is ignored (`session.call` stays `None`).
* **Repeating a side effect.** The server refuses an identical `side_effect` call (same
  arguments) that the model repeats in a later response of the same turn, answering
  `already_done` without calling your handler: a second booking is almost never wanted.
  `@agent.tool(kind="side_effect", repeatable=True)` lets such a repeat through, for a tool where
  doing the same thing twice is legitimate (pressing the same IVR digit twice). `repeatable`
  needs a `side_effect` tool (`ValueError` on a read-only one, like `confirm`).
* **Slow handlers** never delay tool results or command results (they are handled on the reader),
  but they delay later events of the same call. The per-call handler queue is bounded
  (`Agent(event_queue=1024)`); when it overflows the newest events are dropped and the handlers
  get an `EventsLost` in front of the next event after the gap, exactly like the server's own
  backpressure policy. `session.end` is never dropped.
* **Session end.** `on_session_end` runs once per call; if the link dropped without
  `session.end` the reason is `"link_closed"`. When the link drops every running tool sees `ctx.cancelled` (reason
  `"link_closed"`); `read_only` tools are cancelled, `side_effect` tools get
  `Agent(side_effect_grace=30)` seconds to wind down before they are cancelled.
* **Limits.** `Agent(max_sessions=256)`; more calls are answered with `session.reject`
  (`unavailable`). The handshake handler must finish inside `handshake_timeout` (1.5 s; the server
  gives up at 2 s).

## Reconnecting (`resume`)

By default a lost link ends the call for the handlers (`on_session_end`, reason `"link_closed"`),
and the server does not reopen it. Opt in with `Agent(resume=True)` when the agent can keep a
call's state across a dropped connection (the protocol's "Reconnecting", `docs/AGENT_PROTOCOL.md`):

```python
agent = Agent("luigi", resume=True, resume_window=15.0)

@agent.on_session_start
async def started(session):           # once per call, never for a resumption
    session.data["notes"] = []

@agent.on_resumed
async def back(session, event):       # attempt, previous_seq, down_ms
    log.info("link back after %d ms", event.down_ms)
```

* `session.accept` then carries `"resume": true`. When the link is lost the `Session` is *parked*:
  `session.data`, the queued events and the handlers' view of the call stay. Commands raise
  `SessionClosed` meanwhile (`session.parked` is true, `session.closed` too); commands waiting for a
  result fail at the loss, and running tools see `ctx.cancelled` (reason `"link_closed"`) as
  usual. A `side_effect` call that was in flight has an unknown outcome on the server's side and is
  never repeated.
* The server's next `session.start` carries `resume` (`attempt`, `previous_seq`, `down_ms`; its `seq`
  continues the call's count). The SDK answers `session.accept` with `resume: true` and the same
  event subscription, re-attaches the parked session to the new link and runs `on_resumed` after
  the events queued before the loss. There is no greeting and no `on_session_start`;
  `tools`, `system_prompt` and `context` are ignored by the server on a resumption, so the first
  declaration stands. `session.resumes` counts the resumptions of the call.
* What the server could not deliver meanwhile is not replayed: it reports the number as
  `events.lost` (so `on_events_lost` runs). An agent that needs the conversation reads it from its
  own store or asks for it with `update_context`.
* If the server does not come back within `resume_window` seconds (default 15, the server's own
  reconnect window) the call ends for the handlers once (`on_session_end`, `"link_closed"`) and the
  parked state is dropped; a later resumption is answered with `session.reject` `unknown_session`.
  A `session.end` from the server is final at once. Stopping the server (`AgentServer.close`)
  ends parked calls immediately.
* A resumption that arrives while the old link still looks open (half-open TCP) takes the session
  over and closes the old link.
* The state lives in this process. An agent that restarts has nothing to resume: it answers with
  `unknown_session` and the server goes on without it. Keep the state in a shared store if it must
  outlive the process (and route a resumption to the process that holds the call).

## Session tokens for browsers

The server's browser-facing listeners (WebRTC, RTVI, generic WebSocket) accept short-lived,
single-use tokens your backend signs with `BRISVOX_SESSION_SECRET` (`docs/SECURITY.md`):

```python
from brisvox_agent.session_token import mint_session_token

token = mint_session_token(os.environ["BRISVOX_SESSION_SECRET"], "brisvox-webrtc",
                           ttl=60, agent="support", max_s=600, meta={"user": "u-42"})
```

`ttl`, `max_s` and `now` are whole **seconds**: `ttl` is 1 to 86400 (the server refuses a token that
lives longer than 24 hours, which would also fill its single-use set), `max_s` 1 to 86400, and a `now`
that looks like milliseconds raises `ValueError`.

## Examples

* `examples/minimal.py`: the Python twin of the minimal agent in the protocol doc.
* `examples/restaurant.py`: typed tools (enum, nested dataclass, `side_effect` with an
  idempotency key), per-call setup, and every command.
* `examples/resumable.py`: `resume=True`, `on_resumed`, per-call state that survives a dropped
  link.

## Tests

```
python -m venv .venv && .venv/bin/pip install -e '.[test]'
.venv/bin/python -m pytest
```

* `tests/test_protocol.py`: message (de)serialization against the examples and tables of
  `docs/AGENT_PROTOCOL.md` (it reads the doc: error codes, event names and limits must match).
* `tests/test_tools.py`: schema generation and argument conversion.
* `tests/test_agent.py`, `tests/test_examples.py`: the agent over a real WebSocket against a fake
  brisvox server (`tests/fake_server.py`) that replays the doc's sequences.
* `tests/test_resume.py`: reconnecting (parking, resumption, expiry, takeover of a half-open link).
* **Against the real Rust link:** `crates/brisvox-agent-link/tests/python_sdk.rs` runs a call
  (loopback transport, mock providers) linked to `examples/restaurant.py` in a Python process:
  greeting, a `side_effect` and a `read_only` tool, and an agent-initiated `end`. It runs when
  `BRISVOX_PYTHON_SDK` names a Python (3.11+) that has `websockets`. `scripts/verify.sh` finds one,
  runs the whole Python suite (`python -m unittest discover -s tests -t tests` in `sdk/python`)
  and sets the variable for the Rust test. A second test of the file cuts the link of a call in
  the middle (a TCP proxy drops the sockets) and checks that `examples/resumable.py` takes the
  call up again: state kept, `seq` continuing, tools still working. CI sets
  `BRISVOX_REQUIRE_PYTHON_SDK=1`, so there a missing Python fails the gate instead of skipping it:

  ```
  BRISVOX_PYTHON_SDK=.venv/bin/python cargo nextest run -p brisvox-agent-link --test python_sdk
  ```
