# Tools

A tool is a function the LLM can call during a turn: look up an order, book a slot, transfer the
call. brisvox's tool contract is built around the two things that go wrong in voice: a tool that
runs when it should not, and a caller who is kept waiting in silence.

## Kinds

- **`read_only`** tools only look things up. They may run on a *speculative* LLM start, they are
  cancelled when the caller interrupts, and running one twice is harmless.
- **`side_effect`** tools change the world (book, cancel, transfer, charge). They **never run
  speculatively** (they are held until the turn is committed and discarded if the speculation is
  cancelled), **never run twice** (the idempotency key is derived from the session, the turn epoch
  and the call id; a replay of that call returns the recorded result, and an identical call in
  a later response of the same turn is refused as `already_done`; a later turn or another call
  is not covered, so use your own idempotency key for rules such as "one refund per order,
  ever"), and **are not cancelled** by an
  interruption: they complete, are recorded, and the outcome is reported to the LLM on the next
  turn.

## Lifecycle of a call

1. The LLM streams a tool call; the argument deltas are buffered until complete. Partial JSON
   never executes.
2. The arguments are validated against the tool's JSON Schema. A failure goes back to the LLM as
   a structured error, with bounded retries and then a spoken fallback.
3. Independent calls run in parallel; results are appended to the context in the order the LLM
   issued them.
4. If a tool has not answered after a delay (1.2 s by default, or the tool's own
   `filler_after`), the agent says a filler phrase so the line is not silent.

## Where tools come from

- **Built-in telephony tools**: `transfer_call`, `send_dtmf`, `end_call`, over the carrier's REST
  call control ([Telephony](../telephony.md)).
- **In-process tools**: Rust tools registered through `brisvox_server::tools::AppTools`; an agent
  file lists, by name, the ones the LLM may see. Your own binary is one line,
  `brisvox_server::app::run_with_tools(Arc::new(MyTools))`; `examples/rust-tools` is a complete
  project.
- **Remote tools** in your own process, through the
  [Python or TypeScript SDK](../quickstart/sdk-agent.md) and the
  [agent protocol](../reference/agent-protocol.md).

Read more: the [Tools reference](../reference/tools.md) (the contract, the tool engine, filler
and interruption semantics, per-request LLM metrics) and
[Configuration](../configuration.md) (the `tools` key of an agent file).
