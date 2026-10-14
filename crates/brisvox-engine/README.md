# brisvox-engine

The production voice pipeline, one session per call: transport pump, VAD, STT, the turn driver, the LLM runner, TTS and transport out.

This crate is part of [brisvox](https://github.com/bilal07karadeniz/brisvox), a Rust engine for real-time voice agents: phone calls and
browser sessions where a caller talks, the agent listens, thinks (an LLM, with tools) and answers,
and the caller can interrupt at any moment. The crates are versioned and released together; the
whole picture (architecture, configuration, deployment, benchmarks) is in the
[book](https://bilal07karadeniz.github.io/brisvox/) and the [repository](https://github.com/bilal07karadeniz/brisvox).

## License

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or the MIT
license ([LICENSE-MIT](LICENSE-MIT)) at your option.
