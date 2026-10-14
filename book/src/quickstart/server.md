# Run the server with an example agent

`brisvox-server` loads one agent definition and serves every call with it. This page builds the
server, runs it with `examples/agents/support-en.toml` (Deepgram + OpenAI + Cartesia) and plays
a recorded utterance at it.

## 1. Build

```sh
git clone https://github.com/bilal07karadeniz/brisvox && cd brisvox
cargo build --release -p brisvox-server --features all-providers,smart-turn-ort,demo-tools
python3 scripts/models/fetch_models.py     # the Smart Turn model, SHA-256 pinned, into models/
```

- `all-providers` compiles every STT/LLM/TTS adapter. The default build has none; a definition
  that names a provider whose feature is off is refused at startup with the feature to turn on.
- `smart-turn-ort` loads the Smart Turn v3.2 end-of-turn model (the example agent uses the
  adaptive turn policy). The ONNX Runtime binaries are downloaded at build time.
- `demo-tools` adds a canned `lookup_order` tool so the example runs without an application.
  Never ship it to callers. Your own tools come from [an SDK agent](sdk-agent.md) or from
  `brisvox_server::tools::AppTools` in a small Rust binary of your own
  (`examples/rust-tools`).

The Silero voice-activity model is compiled into the binary.

## 2. Run

Check the agent file first. `check` runs the server's own loader without starting anything (no
sockets, no network), so a typo in a key, a missing environment variable, a provider the binary
was built without or a tool nothing provides shows up now, with the line and column, instead of
when the server starts:

```sh
export DEEPGRAM_API_KEY=...  OPENAI_API_KEY=...  CARTESIA_API_KEY=...
export BRISVOX_AGENT_FILE=examples/agents/support-en.toml
target/release/brisvox-server check "$BRISVOX_AGENT_FILE" --require-secrets
```

It exits 0 when the file is valid and 1 when it has errors; without `--require-secrets` a
variable that is not set is only a warning (use that where the keys are not present, such as a
build job). Then start the server:

```sh
target/release/brisvox-server
```

With no other listener configured the server listens for the
[websocket protocol](../reference/websocket-protocol.md) on `127.0.0.1:8080` and for health
probes on `0.0.0.0:8081` (`GET /healthz`, `GET /readyz`, and `GET /metrics` unless `BRISVOX_METRICS` turns it off; off a loopback address `/metrics` needs `BRISVOX_METRICS_TOKEN`). The websocket
listener has no authentication of its own: it refuses to bind a non-loopback address without `BRISVOX_SESSION_SECRET` or `BRISVOX_WS_TOKEN` (see [Security](../operations/security.md)), and a TLS-terminating proxy belongs in front of it.

```sh
curl -i http://127.0.0.1:8081/readyz
```

## 3. Talk to it

One websocket connection is one call: binary frames carry audio, text frames carry JSON
(`start`, `started`, `clear`, `mark`, `stop`, ...). Any client that follows the
[protocol](../reference/websocket-protocol.md) works. The repository ships two ways to try it
without writing one:

- The benchmark harness plays a recorded utterance in real time and reports how long the agent
  took to answer (this needs the three keys, because the providers are real):

  ```sh
  cargo run --release -p brisvox-bench -- v2v --url ws://127.0.0.1:8080/ \
      --fixtures en_two_utterances --runs 1
  ```

- The browser assistant in `examples/web-assistant` (one static HTML file) talks to the server
  over WebRTC with your microphone. It needs a server built with the `webrtc` feature and
  `BRISVOX_WEBRTC_ADDR`; see its README.

## 4. Change the agent

The agent file is the whole configuration: prompt, language, which tools the LLM may call, the
turn and interruption settings, and one provider per stage. Copy `support-en.toml`, edit it, and
restart. `examples/agents/` also has a Turkish support agent, a telephony agent and a provider
failover agent, each as TOML and as YAML. Every key is documented in
[Configuration](../configuration.md); every server setting (`BRISVOX_*` environment variables:
listeners, admission limits, metrics, recording) in [Operations](../operations.md).

## Next

- [Write an agent with the SDK](sdk-agent.md) to put your own tools behind the call.
- [Telephony](../telephony.md) to answer real phone calls.
