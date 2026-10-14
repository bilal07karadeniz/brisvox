# brisvox-inference

The dedicated CPU inference pool and the on-device models of brisvox: Silero VAD and the Smart Turn end-of-turn detector.

The Silero VAD graph (MIT, see `assets/NOTICE`) is embedded in the crate behind the `silero-tract` feature; Smart Turn runs on ONNX Runtime behind `smart-turn-ort` and its model is fetched separately (see the repository's `scripts/models/fetch_models.py`).

This crate is part of [brisvox](https://github.com/bilal07karadeniz/brisvox), a Rust engine for real-time voice agents: phone calls and
browser sessions where a caller talks, the agent listens, thinks (an LLM, with tools) and answers,
and the caller can interrupt at any moment. The crates are versioned and released together; the
whole picture (architecture, configuration, deployment, benchmarks) is in the
[book](https://bilal07karadeniz.github.io/brisvox/) and the [repository](https://github.com/bilal07karadeniz/brisvox).

## License

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or the MIT
license ([LICENSE-MIT](LICENSE-MIT)) at your option.
