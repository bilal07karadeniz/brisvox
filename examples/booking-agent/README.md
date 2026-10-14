# Tool-heavy booking agent (Python SDK)

A phone/voice assistant for a fictional hair salon whose application logic lives in **your own
process**: the brisvox server runs the call (audio, speech recognition, turn taking, the LLM,
speech synthesis) and connects out to this Python agent over the agent protocol
(`docs/AGENT_PROTOCOL.md`). The agent declares five typed tools, answers the LLM's tool calls and
sets the prompt (with today's date) per call.

| File | What it is |
|---|---|
| `agent.toml` | The server side: Deepgram + OpenAI + Cartesia, the filler policy for slow tools, and a fallback prompt for when the agent process is down. |
| `booking_agent.py` | The agent (`brisvox-agent` SDK): tools, per-call prompt, `build_agent(store)`. |
| `booking_store.py` | The salon's rules and calendar: pure Python, injectable clock, thread-safe. Stands in for your booking API. |
| `test_booking_agent.py` | Tests of the rules, the tools and a whole call over the protocol. Run without keys and without a server. |

## The tools

| Tool | Kind | What it does |
|---|---|---|
| `list_services` | `read_only` | The menu: service, minutes, price. |
| `check_availability(service, day)` | `read_only` | Free start times (first twelve and how many more) of a service on a date. |
| `find_booking(confirmation_code)` | `read_only` | Looks a booking up. |
| `book_appointment(service, day, start, customer_name, phone?)` | `side_effect` | Books a slot; answers with a confirmation code. |
| `cancel_booking(confirmation_code)` | `side_effect` | Cancels a booking. |

The parameters are typed Python (`enum.Enum`, `datetime.date`, `str | None`,
`Annotated[T, "description"]`); the SDK turns them into the JSON schema the LLM sees, converts the
arguments back, and keeps `ctx` out of the schema.

### Why two kinds

* **`read_only`** tools may run **speculatively** (with `[speculation]` enabled the engine can
  start them before the caller has finished) and are **cancelled** when the caller barges in.
  They must have no effect, which is true of the three lookups.
* **`side_effect`** tools **never** run speculatively and **never run twice** for one tool call
  (the engine keeps an idempotency key per call), and a barge-in does not cancel them: a booking
  the caller interrupted still happens, and the LLM is told about it as a late note. The tools
  take `ctx: ToolContext` and use `ctx.idempotency_key` themselves: the same key always gets the
  same booking back (`already_booked: true`), so even a redelivered request (a link that dropped
  and reconnected) books once. Cancelling is idempotent by state (`already_cancelled`). The
  key identifies one tool call of one phone call: the same customer phoning twice, or asking
  again in a later turn, produces a new key, so the "one booking per slot" rule below is enforced
  by the store, not by the key.

The store adds a second guard that does not rely on keys: the same customer asking for the same
slot twice in different words is not booked twice, and a slot that was taken between "is it free?"
and "book it" is refused with a message the LLM can act on ("That time is not available. Check
availability and offer another start.").

### What the LLM is told when a tool fails

`raise ToolError("...")` sends that message to the LLM verbatim, so every refusal in
`booking_store.py` is written to be said to a caller. Any other exception tells the LLM only
`tool failed: <ExceptionType>`; the details stay in your log, never on the phone line.

## Run it

1. Install the SDK (Python 3.11+, one dependency, `websockets`):

   ```sh
   pip install ./sdk/python
   ```

2. Start the agent (it needs no keys). It listens on `127.0.0.1` and, with `BRISVOX_AGENT_TOKEN`
   set, requires that bearer token from the server:

   ```sh
   python3 examples/booking-agent/booking_agent.py --port 8765
   ```

   Set `SALON_TZ` to the salon's IANA time zone (for example `SALON_TZ=Australia/Sydney`; on a
   host without a tz database, such as Windows or a slim container image, also `pip install -r examples/booking-agent/requirements.txt`, which installs `tzdata`). Opening hours, "today" in the prompt and
   the one hour lead time are the salon's local time, and a container runs in UTC by default;
   unset, the host's local time is used, which is right only if the process runs in the salon's zone.

3. Start the server with the definition and the agent's URL. It needs the three provider keys
   of `agent.toml` and a build with the providers:

   ```sh
   cargo build --release -p brisvox-server --features all-providers,smart-turn-ort
   BRISVOX_AGENT_FILE=examples/booking-agent/agent.toml \
   BRISVOX_AGENT_URL=ws://127.0.0.1:8765 \
   DEEPGRAM_API_KEY=... OPENAI_API_KEY=... CARTESIA_API_KEY=... \
   ./target/release/brisvox-server
   ```

   (Fill in the Cartesia voice id in `agent.toml` first.) The server now hosts the generic
   websocket listener on `127.0.0.1:8080` (`docs/WEBSOCKET_PROTOCOL.md`); every call opens a link
   to the agent. To talk to it, use any transport: the browser page of `../web-assistant` (give the
   server `BRISVOX_WEBRTC_*` as there and this `BRISVOX_AGENT_FILE`), or a phone number as in
   `../phone-receptionist`. The agent says its greeting first (the SDK's `greeting`), which an
   agent definition alone cannot do.

**As a container.** `Dockerfile` here packages the agent; it vendors the unpublished SDK by
installing the copied `sdk/python`, so build it from the repository root:

```sh
docker build -f examples/booking-agent/Dockerfile -t booking-agent .
docker run --rm -p 8765:8765 -e BRISVOX_AGENT_TOKEN=change-me -e SALON_TZ=Australia/Sydney booking-agent
```

It binds `0.0.0.0` inside the container and runs as a non-root user; the server then uses
`BRISVOX_AGENT_URL=ws://<container name or host>:8765` and the same `BRISVOX_AGENT_TOKEN`
(`docs/DEPLOY.md`, "Your tools service" on a PaaS).

If the agent process is down or slow, the call still starts, on the fallback prompt of
`agent.toml` (the caller hears up to `BRISVOX_AGENT_CONNECT_TIMEOUT_MS` +
`BRISVOX_AGENT_HANDSHAKE_TIMEOUT_MS`, 4 s by default, of silence first).

Try a conversation: "I'd like a haircut on Friday morning" (the LLM calls `check_availability`,
offers times), "ten thirty, my name is Ann" (`book_appointment`; it reads the confirmation code
back), "actually cancel that" (`cancel_booking`).

## Make it yours

* **Swap the store.** `BookingStore` is the only part that knows a calendar. Replace its methods
  with calls to your booking system and keep the contract: an idempotency key in, the same answer
  on a repeat, a `BookingError` with a sayable message on a refusal. Pass the key to the real API
  (an `Idempotency-Key` header) when it supports one.
* **Plain `def` tools** run on a worker thread, so a blocking client does not stall the event
  loop (the store is thread-safe for that reason).
* **Timeouts.** `timeout_ms` is how long the server waits (default 5 s, at most 30 s); a
  `side_effect` call that outlives it keeps running and its result reaches the LLM as a late
  note. The definition's `[filler]` controls the "one moment" phrase for a slow tool (with
  `per_tool = true` each tool's own `filler_after_ms` counts).
* **More of the SDK**: events, `session.say / transfer / end / update_context` in
  `sdk/python/examples/restaurant.py` and `sdk/python/README.md`.

## Tests

```sh
PYTHONPATH=sdk/python python3 -m unittest discover -s examples/booking-agent -p 'test_*.py'
```

They cover the opening hours, the 30 minute grid and the 90 minute colour that must not run into
another appointment, a booking that was retried, two customers for one slot, cancelling twice, the
refusals as the LLM would read them, the declarations (kinds, `required`, enum, `ctx` kept out
of the schema) and one whole call through the SDK's fake brisvox server (the prompt carries the
date; two deliveries of one booking give one booking). `scripts/verify.sh` runs them in its
"examples" stage; with no Python 3.11+ and `websockets` it skips the stage with a warning
(`BRISVOX_REQUIRE_EXAMPLES=1` makes that an error, as CI sets it). The Rust test
`crates/brisvox-server/tests/examples.rs` loads `agent.toml` through the server's config loader.
