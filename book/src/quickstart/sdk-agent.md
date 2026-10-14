# Write an agent with the Python or TypeScript SDK

The SDKs implement the [agent protocol](../reference/agent-protocol.md). The split:

- **The server** runs the call: audio, voice-activity detection, speech recognition, turn taking,
  the LLM, speech synthesis, interruptions.
- **Your agent process** holds the application logic: the tools the LLM can call, the prompt per
  call, commands such as "say this", "transfer the call" or "hang up", and the events you want to
  react to (what the caller said, what the bot said, turns, errors).

The server connects **out** to your agent over a WebSocket, one connection per call, so your
process is a plain WebSocket server you can run anywhere the brisvox server can reach. Set
`BRISVOX_AGENT_URL` (`ws://` or `wss://`) on the server; `BRISVOX_AGENT_TOKEN` is sent as a
bearer token. Without `BRISVOX_AGENT_URL`, calls run on the agent file alone.

## Python

Python 3.11 or newer, one dependency (`websockets`).

```sh
pip install ./sdk/python
```

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

agent.run(port=8765)
```

Tools default to `read_only`; mark a tool that changes the world with
`@agent.tool(kind="side_effect")`. The server never runs those speculatively and never twice
for one call id of a call (the tool context carries an idempotency key). That key does **not**
see a second phone call or a later turn: for "one refund per order, ever" derive your own key
from the business data (the order id) and enforce it in your store or with the downstream
API's `Idempotency-Key`. More in `sdk/python/README.md`; a larger,
tested example is `examples/booking-agent`.

## TypeScript

Node 20 or newer, one runtime dependency (`ws`). The package is not on npm yet; build it from the
repository.

```sh
cd sdk/typescript && npm ci && npm run build
```

```ts
import { Agent, s } from "@brisvox/agent";

const agent = new Agent({ name: "luigi", systemPrompt: "You are Luigi's host.", greeting: "Hi, Luigi's here!" });

agent.tool({
  name: "opening_hours",
  description: "Opening hours of the restaurant on a weekday.",
  parameters: s.object({ day: s.enum(["mon", "tue", "wed", "thu", "fri", "sat", "sun"]) }),
  handler: async ({ day }) => (day === "mon" ? "Closed on Monday." : "12:00 to 22:00."),
});

agent.onUserText(async (session, event) => {
  if (event.text.toLowerCase().includes("bye")) {
    await session.end({ say: "Goodbye!" });
  }
});

await agent.run({ port: 8765 });
```

More in `sdk/typescript/README.md`. The TypeScript SDK mirrors the Python one.

## Connect the server to it

Start the agent, then the server (the same build and keys as in
[Run the server with an example agent](server.md)):

```sh
python sdk/python/examples/minimal.py        # listens on ws://127.0.0.1:8765
BRISVOX_AGENT_URL=ws://127.0.0.1:8765 \
BRISVOX_AGENT_FILE=examples/agents/support-en.toml \
  target/release/brisvox-server
```

The agent file still names the providers and the turn settings; the agent process adds its tools
to the call and may set the prompt per call. An agent that is down, slow or rejects a call never
fails the call: it runs on the agent file's own prompt and tools (and the caller hears up to 4 s
of silence while the server waits for an unreachable agent, with the default timeouts). The wire format, version negotiation, error codes and limits are in
[Agent protocol](../reference/agent-protocol.md); tools in general are in
[Tools](../concepts/tools.md).
