# Introduction

brisvox is a Rust-native, open-source engine for real-time voice agents: a phone call or a
browser session in which a caller talks, the agent listens, thinks (an LLM, with tools) and
answers in a synthesised voice, and the caller can interrupt at any moment.

It is built to beat Pipecat on three axes, with the *same* speech and language providers:

- **Latency.** Voice-to-voice latency is the time from the end of the caller's speech to the
  first sound of the answer. brisvox keeps its own share of that to well under a millisecond per
  stage and takes work off the critical path: connections are prewarmed, the speech-to-text
  `Finalize` runs in parallel with the end-of-turn model, the LLM can start speculatively, and
  the first clause of the answer goes to the voice after one word.
- **Density.** One process hosts many calls. Every call is an isolated session; a panic or a
  provider failure in one never touches another. Model inference runs on a dedicated pool, never
  on the async workers.
- **Correctness.** Interruptions and tool calls are where voice agents silently break. brisvox
  numbers every user turn (a *turn epoch*) and drops stale frames everywhere; after a barge-in
  the LLM sees only the text that was actually played; side-effecting tools never run
  speculatively and never run twice within a call's turn (the exact scope is in the Tools
  chapter: a second call or a later turn is not covered, so keep your own idempotency key for
  "once per order, ever"). Behaviour is proven by deterministic, paused-time
  simulation, not by hope.

> **Status.** The engine, the transports, the provider adapters, the server binary and both
> agent SDKs are implemented and tested against mocks and recorded protocol fixtures. Live
> runs against the real providers, and the head-to-head comparison with Pipecat, are pending:
> see [Benchmark results](benchmarks.md) for exactly what has been measured so far and on what.

## How the book is organised

| Part | What is in it |
|---|---|
| [Quickstart](quickstart.md) | Run the server with an example agent, write an agent in Python or TypeScript, run it all with Docker compose. |
| [Concepts](concepts/pipeline.md) | The mental model: the pipeline, turns and interruptions, played-text truncation, tools. Short pages that point into the reference. |
| [Configuration](configuration.md), [Telephony](telephony.md), [Operations](operations.md) | Agent definition files, phone carriers, running the server in production. |
| [Benchmarks](benchmarks.md) | The numbers that exist, where they come from, and what they do not show. |
| [Reference](reference/architecture.md) | Architecture and design documents, the protocols, and one page per provider. |

The configuration, telephony, operations and reference chapters are the repository's `docs/`
files, included as they are, so there is one source of truth: the same text is on GitHub and in
this book. References in those files to source paths (`crates/...`, `docs/...`,
`bench/results/...`) name files in the repository.

## Where to start

- To see it work: [Run the server with an example agent](quickstart/server.md).
- To put your own application logic behind a call:
  [Write an agent with the SDK](quickstart/sdk-agent.md).
- To understand why it behaves the way it does: [The pipeline](concepts/pipeline.md).

## License

Dual-licensed under Apache-2.0 or MIT, at your option. The speech test fixtures in `fixtures/`
(Google FLEURS / FLORES) are CC BY-SA 4.0; see the repository's `fixtures/NOTICE`.
