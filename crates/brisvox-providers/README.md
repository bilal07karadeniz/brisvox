# brisvox-providers

STT, LLM and TTS traits and their adapters: Deepgram, AssemblyAI, Gladia, OpenAI, Anthropic, Gemini, Cartesia, ElevenLabs, OpenAI Realtime, Gemini Live and local engines.

Every adapter is behind a `provider-*` Cargo feature; `all-providers` turns them all on.

This crate is part of [brisvox](https://github.com/bilal07karadeniz/brisvox), a Rust engine for real-time voice agents: phone calls and
browser sessions where a caller talks, the agent listens, thinks (an LLM, with tools) and answers,
and the caller can interrupt at any moment. The crates are versioned and released together; the
whole picture (architecture, configuration, deployment, benchmarks) is in the
[book](https://bilal07karadeniz.github.io/brisvox/) and the [repository](https://github.com/bilal07karadeniz/brisvox).

## License

Licensed under either of Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or the MIT
license ([LICENSE-MIT](LICENSE-MIT)) at your option.
