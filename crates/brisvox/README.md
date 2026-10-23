# brisvox

A Rust engine for real-time voice agents: phone calls and browser sessions where a caller talks,
the agent listens, thinks (an LLM, with tools) and answers, and the caller can interrupt at any
moment. This crate is the one-dependency way in: it re-exports the server, the tools API and the
core types, and forwards the server's Cargo features.

```sh
cargo add brisvox --features all-providers
```

A voice agent server with your own Rust tools is a three-line `main`:

```rust
use std::sync::Arc;

fn main() -> std::process::ExitCode {
    brisvox::run_with_tools(Arc::new(MyTools::default()))
}
```

`MyTools` implements `brisvox::AppTools` and registers `Tool`s from `brisvox::agent::tools`; the
agent itself (providers, prompt, tools by name) is one TOML or YAML file. Start from the
[Rust tools example](https://github.com/bilal07karadeniz/brisvox/tree/main/examples/rust-tools).

| Path | Crate |
|---|---|
| `brisvox::server` | [`brisvox-server`](https://crates.io/crates/brisvox-server): sessions, config, transports, `app::run_with_tools` |
| `brisvox::agent` | [`brisvox-agent`](https://crates.io/crates/brisvox-agent): conversation logic, the tools API |
| `brisvox::providers` | [`brisvox-providers`](https://crates.io/crates/brisvox-providers): speech and LLM adapters |
| `brisvox::core` | [`brisvox-core`](https://crates.io/crates/brisvox-core): frames, the injected clock, turn epochs |

## Features

Nothing is on that you did not ask for, except the defaults of `brisvox-server` minus its
`mimalloc` (the allocator of the server's own binary; a library leaves the global allocator to
your program):

* Default: `yaml` (YAML agent files), `silero-tract` (the bundled VAD).
* Providers: `all-providers`, or any of `provider-deepgram`, `provider-assemblyai`,
  `provider-gladia`, `provider-sherpa-onnx`, `provider-openai-realtime-stt`, `provider-openai`,
  `provider-anthropic`, `provider-gemini`, `provider-openai-compatible`, `provider-cartesia`,
  `provider-elevenlabs`, `provider-deepgram-aura`, `provider-openai-speech`.
* Transports and extras: `webrtc` (browsers; builds libopus, needs cmake and a C compiler), `sip`,
  `call-control` (telephony tools), `mcp` (MCP tools), `smart-turn-ort` (adaptive turns on ONNX Runtime), `otlp`,
  `recording-encryption`, `schema`, `demo-tools`.

To run the stock server instead of embedding it:
`cargo install brisvox-server --features all-providers`.

This crate is part of [brisvox](https://github.com/bilal07karadeniz/brisvox). The crates are
versioned and released together; the whole picture (architecture, configuration, deployment,
benchmarks) is in the [book](https://bilal07karadeniz.github.io/brisvox/) and the
[repository](https://github.com/bilal07karadeniz/brisvox).

## License

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or the MIT
license ([LICENSE-MIT](LICENSE-MIT)) at your option.
