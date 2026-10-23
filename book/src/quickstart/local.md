# A fully local assistant

A voice assistant that never leaves your machine: no API key, no cloud. Speech recognition is
**sherpa-onnx** (on the CPU), the language model is **Ollama**, the voice is **Kokoro**
(Kokoro-FastAPI, on the GPU when you have one), and you talk to it from a browser page that
`brisvox-server` serves itself. The agent is
[`examples/agents/local/local-en.toml`](https://github.com/bilal07karadeniz/brisvox/blob/main/examples/agents/local/local-en.toml).

**The short way.** If you have Docker, one command starts all of it (the recognizer, Ollama, the voice
and the server, CPU only) and serves the same browser page; nothing to build or install:
`docker compose -f deploy/local/compose.yaml up`, then open `http://localhost:8088/demo?force=ws`.
See [`deploy/local/README.md`](https://github.com/bilal07karadeniz/brisvox/blob/main/deploy/local/README.md).
The steps below are the same stack by hand, on a GPU if you have one.

What runs where:

```
browser (microphone) --WebSocket--> brisvox-server --ws--> sherpa-onnx   :6007  (STT, CPU)
                                           |------http--> Ollama         :11434 (LLM)
                                           '------http--> Kokoro-FastAPI :8880  (TTS, GPU or CPU)
```

The brisvox server and Kokoro (published with `-p 127.0.0.1:8880:8880`) bind `127.0.0.1`, and
Ollama does by default. **sherpa-onnx's `streaming_server.py` does not:** it has no bind-address
flag and listens on all interfaces (its source passes `host=""` to `websockets.serve`, checked
2026-10-14). The three helper servers have no authentication, so on a machine that other computers
can reach, block port 6007 (6006 for Turkish) from outside with the host firewall, or run the
script inside a container that publishes the port only on loopback (`-p 127.0.0.1:6007:6007`).
Nothing in this setup needs a key or an account.

This page is verified in parts: the server, sherpa-onnx (English and Turkish models) and Kokoro
were each run on WSL2 (Ubuntu 24.04) with an NVIDIA RTX 5070, and the whole stack was exercised
in a browser call. The project gives no memory or disk figure for the stack; plan for a few GB of
downloads (the models, the Kokoro image, an Ollama model) and check what your GPU can hold. Tool
calling through Ollama was measured on five local models and works reliably with one of them
(step 2); the example agent itself defines no tools.

## 1. Build the server

```sh
git clone https://github.com/bilal07karadeniz/brisvox && cd brisvox
cargo build --release -p brisvox-server \
  --features provider-sherpa-onnx,provider-openai-compatible,provider-openai-speech,smart-turn-ort,webrtc
python3 scripts/models/fetch_models.py     # the Smart Turn model, SHA-256 pinned, into models/
```

The server looks for that model in `BRISVOX_MODELS_DIR`, whose default `models` is relative to
the directory the server is **started from**, not to the binary. Start it from the repository
root as the commands below do, or set an absolute path in step 3
(`export BRISVOX_MODELS_DIR="$PWD/models"`); `check` warns when the file is not found.

The repository pins its Rust toolchain in `rust-toolchain.toml`; `rustup` installs it on the first
build. Why each feature:

- `provider-sherpa-onnx`, `provider-openai-compatible`, `provider-openai-speech` are the three
  adapters this agent names (`all-providers` turns every adapter on instead).
- **`smart-turn-ort`** loads the end-of-turn model. The agent uses the default, adaptive turn policy;
  without this feature and the model file the server logs a warning and every turn ends at a
  3 second fallback deadline, which feels broken. Keep it.
- **`webrtc`** is what serves the browser page: the page, its signaling and the WebSocket audio path
  live in the WebRTC listener. It compiles libopus, so you need a C compiler and `cmake`
  (`sudo apt install build-essential cmake` on Debian and Ubuntu). If you talk to the server with
  your own websocket client instead, leave it out and use `BRISVOX_WS_ADDR`
  ([protocol](../reference/websocket-protocol.md)).

## 2. Start the three helper servers

Each in its own terminal. These are the commands of the projects themselves, as in the header of
the agent file.

**Speech recognition: sherpa-onnx on port 6007 (English).**

```sh
git clone https://github.com/k2-fsa/sherpa-onnx          # for python-api-examples/
pip install sherpa-onnx websockets numpy
wget https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2
tar xvf sherpa-onnx-streaming-zipformer-en-2023-06-26.tar.bz2
M=./sherpa-onnx-streaming-zipformer-en-2023-06-26
python3 ./sherpa-onnx/python-api-examples/streaming_server.py \
  --tokens  $M/tokens.txt \
  --encoder $M/encoder-epoch-99-avg-1-chunk-16-left-128.int8.onnx \
  --decoder $M/decoder-epoch-99-avg-1-chunk-16-left-128.onnx \
  --joiner  $M/joiner-epoch-99-avg-1-chunk-16-left-128.int8.onnx \
  --port 6007
```

The model prints UPPERCASE text without punctuation; the language model copes with it. For a
Turkish assistant use `examples/agents/local/local-tr.toml`, the `seda-v0.1` model and
`--port 6006` (the commands are in `docs/providers/sherpa-onnx.md`). The ports are fixed by the
two example agents, 6006 for Turkish and 6007 for English, so both stacks can run side by side.

**The language model: Ollama.**

```sh
ollama pull gemma4:12b-it-qat
```

`gemma4:12b-it-qat` is the model that called tools correctly in every test run (it needs about
7.7 GB of GPU memory and answers its first sentence in roughly 0.3 s on an RTX 5070). With less
room, `gemma4:e4b-it-qat` (3.1 GB, even faster) speaks well but fumbles tool calls, so give the
agent no tools. The example agents set `reasoning_effort = "none"`: the models above think by
default, which costs seconds before the first word. If the language model and Kokoro together do
not fit in your GPU memory, Ollama puts part of the model on the CPU and the first sentence takes
about 1.7 s; the Kokoro CPU image frees the GPU for the language model. The numbers for five
models are in [Tools](../reference/tools.md) ("Local models through Ollama").

The Linux package starts `ollama serve` as a service; start it yourself only when nothing listens
on `127.0.0.1:11434`. A model that was idle is unloaded and the next request pays for loading it
(`OLLAMA_KEEP_ALIVE` on the server keeps it resident).

**The voice: Kokoro-FastAPI on port 8880.**

```sh
docker run --gpus all -p 127.0.0.1:8880:8880 ghcr.io/remsky/kokoro-fastapi-gpu:latest
```

On an RTX 50 series card use the tag `latest-cu128` (CUDA 12.8). Without a GPU use the image
`ghcr.io/remsky/kokoro-fastapi-cpu:latest` and drop `--gpus all`. The `127.0.0.1:` in `-p`
matters: Kokoro's own examples publish the port on every interface. The first request after a
long idle is slower (the model has to warm up), so the agent's `kokoro` preset warms it in the
background at the start of a call.

So the compute is split like this: speech recognition runs on the CPU, the language model and the
voice use the GPU when there is one. Turkish needs a different voice server, and the one the
project tried (Chatterbox) takes about 3 seconds to the first audio, too slow for a live turn
(`docs/providers/openai-speech.md`, "Live").

## 3. Check, then run

```sh
target/release/brisvox-server check examples/agents/local/local-en.toml
```

`check` loads the agent file with the server's own loader and opens no sockets, so a typo, a
provider the binary was built without or a missing Smart Turn model shows up now with the line and
column. Then start the server with loopback-only settings:

```sh
export BRISVOX_AGENT_FILE=examples/agents/local/local-en.toml
export BRISVOX_MODELS_DIR="$PWD/models"        # absolute: the default `models` is relative to the working directory
export BRISVOX_WEBRTC_ADDR=127.0.0.1:8088      # the page and the WebSocket audio path
export BRISVOX_HEALTH_ADDR=127.0.0.1:8081      # the default is 0.0.0.0:8081
target/release/brisvox-server
curl http://127.0.0.1:8081/readyz              # ready
```

No credentials are needed: listeners on a loopback address start without them. Nothing here reads
a `.env` file; export the variables in the shell that starts the server.

## 4. Talk to it

Open **<http://127.0.0.1:8088/demo?force=ws>** in Chrome or Edge, press Start, allow the
microphone and say "Hello". Talk over the assistant to interrupt it.

There are two ways to open the page. `/demo` (no query) is the default: the page tries WebRTC
first, and if the media connection is not up after 4 seconds it falls back to a WebSocket on the
same port. `/demo?force=ws` skips WebRTC and sends the audio over that WebSocket from the start.
Use `?force=ws` with a browser on the same machine as the server: WebRTC media between a browser
and a server on one host needs a real network address (Chrome gathers no loopback candidates),
so the WebRTC attempt cannot succeed here and plain `/demo` only adds the 4 second wait (and a
second session token). On a server other people reach over the network, use plain `/demo`:
WebRTC carries the audio there, with the WebSocket as the safety net. The status line says which transport carries the
call. The page `examples/web-assistant/web/index.html` does the same with its own server box if
you want a page you can edit.

## Notes for Linux, WSL2 and Windows

- **Linux.** Everything above runs as written, and nothing leaves loopback, so no LAN address or
  firewall rule is involved.
- **WSL2 on Windows.** This is what the project's own local runs used (Ubuntu 24.04). Run the server,
  sherpa-onnx, Ollama and Kokoro inside WSL2. Docker needs the NVIDIA container toolkit for
  `--gpus all` (Docker Desktop with the WSL2 backend, or Docker inside the distribution). Open the
  page from the Windows browser at `http://127.0.0.1:8088/demo?force=ws`: WSL2 normally forwards
  loopback ports to Windows, and `localhost` counts as a secure context for the microphone. If the
  page does not load, check `localhostForwarding` in `.wslconfig` or use WSL's mirrored networking
  (not tested here).
- **Windows without WSL2.** The server itself builds on Windows (the project is developed there),
  but the helper servers and the commands on this page were run on Linux. Use WSL2 for them.
- **Any host other than your own machine.** A page opened from another computer is not a secure
  context for the microphone, so it needs HTTPS in front, and the server then needs credentials
  (`BRISVOX_SESSION_SECRET`): see [Deploying in a container](../operations/deploy.md) and
  [Security](../operations/security.md). Do not expose the helper servers beyond loopback (see the sherpa-onnx note at the top of this page).

## If it does not work

- **`check` says a provider's feature is off.** The binary was built without the adapter it names;
  rebuild with the `--features` of step 1.
- **The first turn is empty or answered late.** Look at the server log for the STT connection
  (`ws://127.0.0.1:6007` must be the port sherpa-onnx runs on) and at `smart-turn-ort` in the build
  line. The sherpa-onnx notes ("First-turn check" in
  [sherpa-onnx](../reference/providers/sherpa-onnx.md)) explain the padding the adapter sends.
- **Long silence before the first answer after a break.** Ollama loading the model or Kokoro
  warming up; see the notes in step 2.
- **Settings and keys of the agent file:** [Configuration](../configuration.md).
