# brisvox-server

The brisvox voice agent server: hundreds of isolated calls in one process, an HTTP API, health and metrics endpoints, one TOML or YAML file per agent. Binary and library.

`cargo install brisvox-server --features all-providers,call-control,smart-turn-ort` installs the binary; as a library, `brisvox_server::app::run_with_tools` is a whole `main` for a server with your own Rust tools (the [`brisvox`](https://crates.io/crates/brisvox) crate re-exports it with the tools API behind one dependency).

This crate is part of [brisvox](https://github.com/bilal07karadeniz/brisvox), a Rust engine for real-time voice agents: phone calls and
browser sessions where a caller talks, the agent listens, thinks (an LLM, with tools) and answers,
and the caller can interrupt at any moment. The crates are versioned and released together; the
whole picture (architecture, configuration, deployment, benchmarks) is in the
[book](https://bilal07karadeniz.github.io/brisvox/) and the [repository](https://github.com/bilal07karadeniz/brisvox).

## License

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or the MIT
license ([LICENSE-MIT](LICENSE-MIT)) at your option.
