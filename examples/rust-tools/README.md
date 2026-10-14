# Rust tools in your own server binary

A voice agent whose tools are written in Rust and run inside the server process. Use this shape
when your application logic is Rust and you want one binary; use the
[booking agent](../booking-agent) (Python SDK) when it lives in another process or language.

| File | What it is |
|---|---|
| `src/main.rs` | The whole binary: `brisvox_server::app::run_with_tools(Arc::new(MyTools::default()))`. |
| `src/lib.rs` | `MyTools`, an `AppTools` implementation: a read-only `get_time(timezone)` and a side-effect `save_note(text)`, with tests. |
| `agent.toml` | The agent: Deepgram + OpenAI + Cartesia, and `tools = ["get_time", "save_note"]`. |
| `Cargo.toml` | Depends on `brisvox-server` with the provider features `agent.toml` names. |

## Run it

```sh
export DEEPGRAM_API_KEY=...  OPENAI_API_KEY=...  CARTESIA_API_KEY=...
export BRISVOX_AGENT_FILE=examples/rust-tools/agent.toml

# Validate the agent file first: offline, with line and column for every problem.
cargo run -p brisvox-example-rust-tools -- check "$BRISVOX_AGENT_FILE" --require-secrets

# Serve calls (websocket on 127.0.0.1:8080, health on :8081; see docs/OPERATIONS.md).
cargo run -p brisvox-example-rust-tools
```

The binary reads the same environment variables as the stock `brisvox-server`
(`docs/OPERATIONS.md`) and takes the same command line (`check`, `--help`, `--version`). Talk to
it with any client of the [websocket protocol](../../docs/WEBSOCKET_PROTOCOL.md), for example
`cargo run --release -p brisvox-bench -- v2v --url ws://127.0.0.1:8080/ --fixtures en_two_utterances --runs 1`,
and ask what time it is in Istanbul, or ask it to pass a message on.

## The two tools

| Tool | Kind | Why |
|---|---|---|
| `get_time(timezone)` | `ReadOnly` | Changes nothing, so the engine may start it before the caller has finished speaking (speculation) and cancels it on a barge-in. It reads the injected clock (`SharedClock::wall`), never the system clock. An argument it cannot use is a `ToolError` whose message tells the model what to try. |
| `save_note(text)` | `SideEffect` | Changes the world, so the engine never runs it speculatively and never twice for one call, and a barge-in does not cancel it. It still uses `ctx.idempotency_key().digest()` as the key of its effect, as a tool calling a real API must, so a redelivered request returns the same note (`already_saved: true`). |

Rules a tool follows (`docs/TOOLS.md`): no blocking or CPU-heavy work (move it to a blocking
pool), every IO await bounded by a timeout, state that outlives a call behind an `Arc` in your
`AppTools`, per-call data only from `ToolCallCtx`.

## Make it yours

1. Copy this directory next to your own code, or depend on `brisvox-server` from your own crate
   (next section).
2. Replace the tools in `src/lib.rs`. Each is a `Tool` (an async `call(args, ctx)`) plus a
   `ToolSpec` (name, description the model reads, JSON Schema of the arguments, `ToolKind`);
   arguments are validated against the schema before your code runs.
3. Name the tools in your agent file's `tools = [...]`. A name no tool provides is an error of
   `check` and of the server's start.
4. In `Cargo.toml`, enable the `brisvox-server` features your agent file needs: the `provider-*`
   of its three providers, plus `webrtc`, `smart-turn-ort` (for the `adaptive` turn policy),
   `call-control` or `otlp` as you use them. `all-providers` turns on every adapter.
5. Run `check` after every change to the agent file.

## Outside this repository

The brisvox crates are **not on crates.io**; depend on them from git. A standalone project is
this `Cargo.toml` (pin `rev` to a commit you tested; a branch name moves under you):

```toml
[package]
name = "my-voice-agent"
version = "0.1.0"
edition = "2024"
rust-version = "1.99"

[dependencies]
brisvox-server = { git = "https://github.com/bilal07karadeniz/brisvox", rev = "<commit>", features = [
    # The providers your agent file names (docs/CONFIG.md), or "all-providers".
    "provider-deepgram", "provider-openai", "provider-cartesia",
    # Optional, as in the README's "What to build with": "smart-turn-ort" (adaptive turns),
    # "webrtc" (browsers; needs cmake and a C compiler), "call-control" (telephony tools).
] }
# What src/lib.rs imports for the tools: the Tool/ToolSpec types and the injected clock.
brisvox-agent = { git = "https://github.com/bilal07karadeniz/brisvox", rev = "<commit>" }
brisvox-core = { git = "https://github.com/bilal07karadeniz/brisvox", rev = "<commit>" }
serde_json = "1"
```

with `src/main.rs` and `src/lib.rs` copied from this directory. Three more things to copy or know:

* **The toolchain.** Copy `rust-toolchain.toml` from the repository root (it pins Rust 1.99.0);
  the crates use edition 2024 and need at least 1.99.
* **The lockfile.** Copy the repository's `Cargo.lock` next to your `Cargo.toml` once: cargo
  keeps the dependency versions the project was tested with and drops the entries it does not
  use. A fresh resolution picks the newest compatible versions of every dependency instead.
* **A fully local agent** (no API key; sherpa-onnx speech recognition, an OpenAI-compatible LLM
  server such as Ollama, an OpenAI-compatible speech server such as Kokoro, see
  `book/src/quickstart/local.md`) takes these features:
  `provider-sherpa-onnx`, `provider-openai-compatible`, `provider-openai-speech`,
  `smart-turn-ort` and, for a browser, `webrtc`.

`get_time` accepts fixed UTC offsets only (`"UTC+3"`, `"+05:30"`), to keep the example free of a
time zone database; a real application would use `chrono-tz` or `jiff`.

## Tests

```sh
cargo nextest run -p brisvox-example-rust-tools
```

They register the tools, check their kinds, load `agent.toml` and resolve its tool names against
the registry (a renamed tool or a stale agent file fails here), and exercise both tools,
including the idempotency of `save_note`.
