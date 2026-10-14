# Quickstart

Four ways in, from smallest to most complete:

1. [Run the server with an example agent](quickstart/server.md). Build `brisvox-server`, point it
   at `examples/agents/support-en.toml`, set three provider keys, and talk to it over the
   websocket protocol.
2. [Write an agent with the Python or TypeScript SDK](quickstart/sdk-agent.md). The server runs
   the call (audio, speech recognition, turn taking, LLM, speech synthesis) and connects out to
   your process, which holds your tools and your application logic.
3. [Docker and compose](quickstart/docker.md). The container image and `deploy/compose.yaml`:
   a websocket agent and a Twilio phone agent from one image.
4. [A fully local assistant](quickstart/local.md). sherpa-onnx, Ollama and Kokoro on your own
   machine, no API key, talked to from a browser page.

What you need in all cases:

- Rust (the toolchain is pinned in `rust-toolchain.toml`; `rustup` picks it up) for a source
  build, or Docker for the image.
- API keys for one provider per stage (none for [the local assistant](quickstart/local.md)). The examples use Deepgram (speech to text), OpenAI (LLM)
  and Cartesia (text to speech). Keys are read from the environment, never from a file in the
  repository, and the server does not read a `.env` file: export them in the shell (or use
  `--env-file` with Docker).
- For the browser example ([`examples/web-assistant`](https://github.com/bilal07karadeniz/brisvox/tree/main/examples/web-assistant))
  a server built with the `webrtc` feature, which also needs a C compiler and cmake.

Without keys you can still run everything that does not call a provider: the deterministic
simulations and the benchmark harness run on mock providers
(`cargo run --release -p brisvox-bench -- v2v --sim`, see [Benchmark results](benchmarks.md)).
