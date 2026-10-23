# Try brisvox without keys: a voice agent that runs entirely on your computer

One command starts a complete voice agent with **no API key, no account and no cloud**: speech
recognition, the language model and the voice all run in containers on your machine, and you talk
to it in the browser.

```sh
git clone --depth 1 https://github.com/bilal07karadeniz/brisvox && cd brisvox
docker compose -f deploy/local/compose.yaml up
```

When the log says `brisvox-server ready` (the first start needs a while, see below), open
**<http://localhost:8088/demo?force=ws>** in Chrome, Edge or Firefox, press **Start call**, allow
the microphone and say "Hello". Talk over the assistant to interrupt it. `Ctrl+C` stops the stack.

```
browser --WebSocket--> brisvox-server --ws---> sherpa-onnx  :6007   speech recognition (English, CPU)
 127.0.0.1:8088          (the engine)  --http--> Ollama     :11434  Qwen2.5 1.5B Instruct (CPU)
                                       '-http--> Kokoro     :8880   text to speech (CPU)
```

## What you need

- Docker with Compose v2 (Docker Desktop, or Docker Engine with the compose plugin) on Linux,
  macOS or Windows. `linux/amd64` and `linux/arm64` both work (Apple silicon included).
- A computer with **4 or more cores and 8 GB of memory available to Docker**, and about **20 GB of
  free disk** (an estimate: the images are larger unpacked than the download sizes below).
  Everything runs on the CPU; a GPU is not used.
- A microphone and a current browser. The page is on `localhost`, which browsers treat as secure,
  so the microphone works without HTTPS.

## First start

The first `up` downloads and builds everything; the log shows the progress of each part.

| Part | What | First-start download |
|---|---|---|
| `brisvox` | the published `brisvox-server` image, Smart Turn model inside | 68 MB |
| `stt` | built here: sherpa-onnx 1.13.8 on Python, the English streaming zipformer model | 310 MB model archive (75 MB kept), 45 MB Python base image, 32 MB of packages |
| `llm` | Ollama 0.40.8 | 3.4 GB (its image carries GPU libraries this stack does not use) |
| `llm-pull` | the language model `qwen2.5:1.5b` into a Docker volume | 1.0 GB |
| `tts` | Kokoro-FastAPI 0.9.0 CPU image, the voice model inside | 1.5 GB |
| `net` | an empty container that owns the network (see "Security") | 2 MB |

About 6.5 GB in total: plan on 5 to 15 minutes on a fast connection, longer on a slow one. Later
starts reuse all of it and take one to two minutes (the models load into memory). The language
model lives in the volume `brisvox-local_ollama-models`; `docker compose down` keeps it,
`docker compose down -v` deletes it.

The services start in order (`depends_on` with health checks): the three servers, then the model
pull, then brisvox-server, so the page is only served when everything behind it is ready. Check
with `docker compose -f deploy/local/compose.yaml ps` (every service `healthy`, `llm-pull`
`exited (0)`).

## What to expect

It holds a conversation: the speech recognizer is a real streaming model, the language model
follows a short system prompt and keeps the context of the call, the voice is Kokoro, you can
interrupt it, and Smart Turn decides when you are done speaking. It is a 1.5 billion parameter
model on a CPU, so it is **not** as sharp as a cloud model: it answers simple questions and chats
sensibly, gets arithmetic and facts wrong now and then, and cannot look anything up (the system
prompt tells it so). The speech recognizer is trained on English read speech and mishears rare
words and names; its text arrives without punctuation.

The language model is chosen to be fast on a CPU. Measured with Ollama alone on a desktop CPU
limited to 4 threads (one memory channel), a short conversation: `qwen2.5:1.5b` reads it in 0.1 to
0.2 s, writes about 28 tokens per second (a word is about 1.3 tokens) and had its first sentence
ready 0.3 to 1.2 s after the request. Add the speech recognizer's final, Smart Turn's decision and
the voice's first audio; a laptop CPU is slower. Why this model: it is small enough for a CPU,
Apache-2.0 licensed, and in a 14-turn test conversation it answered in short, correctly
capitalized spoken sentences.

### A faster machine, a better model

`agent-quality.toml` uses Llama 3.2 3B (2.0 GB): better answers, about a third of the speed (7 to 13
tokens per second on the same 4-thread limit, first sentence 1 to 4 s after the request).

```sh
BRISVOX_LOCAL_AGENT=agent-quality.toml docker compose -f deploy/local/compose.yaml up
```

The same variable selects any agent file in `deploy/local/agents/`. To use another Ollama model
change the one `model = "..."` line in a copy of `agent.toml` (the compose file reads that line to
know what to pull) and select the copy. A model that thinks before it answers needs its thinking turned off first (a
`reasoning_effort` line, docs/CONFIG.md). To change the prompt or the
voice, edit the file and `docker compose restart brisvox`; the voices and the format of the agent
file are in [`docs/CONFIG.md`](../../docs/CONFIG.md) and
[`docs/providers/openai-speech.md`](../../docs/providers/openai-speech.md).

### Build the server from source

To run the server of your checkout instead of the published image:

```sh
docker compose -f deploy/local/compose.yaml -f deploy/local/compose.build.yaml up --build
```

The first build compiles the server and takes several minutes ([`docs/DEPLOY.md`](../../docs/DEPLOY.md),
"The image").

## Settings

Environment variables (or a `.env` file in `deploy/local/`), all optional:

| Variable | Default | |
|---|---|---|
| `BRISVOX_LOCAL_AGENT` | `agent.toml` | the agent file in `deploy/local/agents/`: `agent.toml` (Qwen2.5 1.5B) or `agent-quality.toml` (Llama 3.2 3B) |
| `BRISVOX_LOCAL_PORT` | `8088` | the port of the page on `127.0.0.1` |
| `BRISVOX_IMAGE` | the pinned release | another `brisvox-server` image |

## Security: what is reachable

Only `127.0.0.1:8088` is published, and only the machine itself can reach it. The three helper
servers have no authentication of their own (sherpa-onnx's server listens on every interface and
has no bind option), and brisvox-server sends plain `http://` and `ws://` only to loopback
addresses. So the helpers and brisvox-server share one network namespace (`network_mode:
"service:net"`): they reach each other at `127.0.0.1`, as in [`examples/agents/local/`](../../examples/agents/local),
and nothing else can reach them, not even other containers or the rest of your network.

The server runs with `BRISVOX_INSECURE_LISTENERS=1`, which lets the page and its WebSocket accept
calls without a token. That is acceptable here only because the port is published on loopback.
**Do not change `127.0.0.1` in the port mapping to `0.0.0.0` or a LAN address**: anyone who could
reach the port could start calls. To use the agent from other machines, put an HTTPS reverse
proxy with authentication in front and give the server session tokens
([`docs/DEPLOY.md`](../../docs/DEPLOY.md), [`docs/SECURITY.md`](../../docs/SECURITY.md)); a page
opened from another computer is not a secure context, so its microphone would not work without
HTTPS anyway. If `net` is recreated, recreate the other services with it
(`docker compose up -d --force-recreate`); `down` followed by `up` always does.

The call is carried over a WebSocket on the same port rather than WebRTC: media over UDP cannot
reach a browser on the same machine through a Docker port mapping, which is why the link above
ends in `?force=ws`. Plain `/demo` also works but waits four seconds for WebRTC before it falls
back. Nothing in this stack contacts a remote service while a call runs; Ollama is told not to use
its cloud features (`OLLAMA_NO_CLOUD`), and the downloads above happen at build and first start.

## If it does not work

- **The page does not open.** Wait for `brisvox-server ready` in the log, or look at
  `docker compose -f deploy/local/compose.yaml ps` for a service that is not `healthy`; its log is
  `docker compose -f deploy/local/compose.yaml logs <service>` (`stt`, `llm`, `llm-pull`, `tts`,
  `brisvox`).
- **The first answer takes a long time, or the status line shows an error.** Out of memory is the
  usual cause: Docker needs roughly 4 GB for the models (6 GB with `agent-quality.toml`; estimates); raise Docker
  Desktop's memory limit. The `llm-pull` job loads the model before brisvox starts, so a slow first
  answer after that is the voice warming up.
- **The microphone is blocked.** Open the page as `http://localhost:8088/...` or
  `http://127.0.0.1:8088/...` (not a LAN name) and allow the microphone.
- **Port 8088 is taken.** Set `BRISVOX_LOCAL_PORT`.
- **Start again from scratch.** `docker compose -f deploy/local/compose.yaml down -v` removes the
  containers and the model volume; `docker image rm brisvox-local/sherpa-onnx:1.13.8` removes the
  built image.
- **The speech recognizer hears nothing or garbage.** It is trained on English read speech; speak
  English, close to the microphone.

## Licences of what runs

Brisvox is MIT OR Apache-2.0. The compose file pulls the other parts from their own registries
and repositories; nothing of them is redistributed here. Versions, digests, sources and the date
they were checked: [`docs/providers/local-speech.md`](../../docs/providers/local-speech.md),
section "The keyless Docker stack".

| Part | Licence |
|---|---|
| sherpa-onnx and its `streaming_server.py` | Apache-2.0 |
| the English streaming zipformer model (`sherpa-onnx-streaming-zipformer-en-2023-06-26`) | Apache-2.0 |
| Ollama | MIT |
| Qwen2.5 1.5B Instruct (`qwen2.5:1.5b`) | Apache-2.0 |
| Llama 3.2 3B (`llama3.2:3b`, only with `agent-quality.toml`) | the **Llama 3.2 Community License** and Meta's acceptable use policy; not an open-source licence. Pulling the model means you accept it: <https://huggingface.co/meta-llama/Llama-3.2-3B-Instruct> |
| Kokoro-FastAPI and the Kokoro-82M weights | Apache-2.0 (the image also carries espeak-ng, GPL-3.0, as a separate program) |
| busybox (the `net` container) | GPL-2.0 |
