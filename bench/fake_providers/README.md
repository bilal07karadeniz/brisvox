# Fake providers

One long-running server that speaks, over TLS, the wire protocols of the three providers a voice
call talks to, so that load-test calls go through the **real provider client code** of both
systems under comparison (brisvox's Rust adapters, Pipecat's Python services: TLS, websocket and
HTTP/SSE framing, JSON parsing) while no API key, no network and no money is involved.

| fake | endpoint | brisvox client | Pipecat 1.12 client |
|---|---|---|---|
| Deepgram live STT | `wss://HOST:8443/v1/listen` | `stt.provider = "deepgram"` (`[stt.deepgram].base_url`) | `DeepgramSTTService(base_url=...)` (deepgram-sdk, websockets) |
| OpenAI-compatible chat | `https://HOST:8443/v1/chat/completions` (HTTP/2 and HTTP/1.1) | `llm.provider = "openai_compatible"` (`base_url`) | `OpenAILLMService(base_url=...)` (openai SDK, httpx) |
| Deepgram Aura TTS | `wss://HOST:8443/v1/speak` | `tts.provider = "deepgram_aura"` (`[tts.deepgram_aura].url`) | `DeepgramTTSService(base_url=...)` (websockets) |

Cartesia is not faked: both systems speak Aura (`bench/pipecat_baseline/agent_live_en_aura.toml`
is the live comparison agent), and Aura is the cheaper protocol to keep faithful.

The server is a Rust workspace member (`brisvox-fake-providers`, `publish = false`): tokio, hyper
(HTTP/1.1 + HTTP/2), rustls with the `ring` provider, tokio-tungstenite. Not the engine: it keeps
the real clock and prints to stderr. 1000 concurrent calls (2000 websockets) are about two cores
on a Windows desktop, see "Capacity of the fakes".

## Run it

```sh
cargo build --release -p brisvox-fake-providers
./target/release/brisvox-fake-providers --fixtures-dir fixtures           # TLS :8443, plain :8080
./target/release/brisvox-fake-providers --help
```

Two listeners (`--tls-addr`, `--plain-addr`; `off` disables one; `$PORT` replaces the plain port):

| port | serves |
|---|---|
| 8443 (TLS) | `wss://` and `https://`; ALPN `h2` and `http/1.1` (a websocket client offers only `http/1.1`, as in production); the three endpoints above, `GET /v1/models`, plus `/healthz`, `/metrics`, `/ca.pem` |
| 8080 (plain) | the same routes over `ws://` and `http://`, so the proxy of a PaaS (Railway healthchecks) and `curl` work without the CA |

`GET /metrics` (Prometheus text): open, peak and total of connections, STT streams, TTS streams
and chat requests; audio bytes in and out; results sent; TLS handshakes and failures; refused
connections; `process_cpu_seconds_total` and `process_resident_memory_bytes`. `--max-connections`
(20000) bounds open sockets; the rest is closed at accept and counted.

## TLS and the CA

TLS is part of the real per-call cost (handshakes, record encryption on 50 audio frames per second
per call), so the fakes use it. At start the server creates a throwaway CA and a server
certificate it signs (ECDSA P-256, valid for `localhost`, `127.0.0.1`, `::1`, `fakes`,
`fake-providers`; replace the list with `--san NAME`, repeatable). The CA certificate is at
`http://HOST:8080/ca.pem` and, with `--ca-dir DIR`, in `DIR/ca.pem` (reused on restart together
with `server.pem` and `server.key`). The CA's key is dropped after signing. The clients trust the
CA; they never skip verification:

| client | how it is told to trust the CA |
|---|---|
| brisvox-server, brisvox-bench | `BRISVOX_EXTRA_CA_FILE=/path/ca.pem` (new, `docs/OPERATIONS.md`; tests and benchmarks only; added to the public roots, applies to every adapter through `brisvox_providers::net::tls_config`) |
| Pipecat bot (websockets, httpx, the Deepgram SDK) | `SSL_CERT_FILE=/path/ca.pem` (OpenSSL's default context; verified for websockets and httpx with Python 3.13's strict verification) |
| plain Python clients (`check_wire.py`) | `--ca /path/ca.pem` |

The host name in the agent file must be one of the certificate names. `agent.fakes.toml` uses
`127.0.0.1:8443`; for another host copy it and replace that string:
`sed 's/127.0.0.1:8443/fakes:8443/' bench/fake_providers/agent.fakes.toml > agent.toml`.

## The agent file

`agent.fakes.toml` is the live comparison agent (`agent_live_en_aura.toml`: Deepgram nova-3 with
300 ms endpointing, an OpenAI-compatible LLM, Aura `aura-2-thalia-en` at 24 kHz, adaptive turns
with Smart Turn) with the three URLs pointing at the fakes. **Both systems read this one file**:
brisvox-server as `BRISVOX_AGENT_FILE`, the Pipecat bot as `bot.py --agent` (`agent_mapping.py`
now maps `[stt.deepgram].base_url` and `[tts.deepgram_aura].url`).

Its key variable is `FAKE_PROVIDERS_API_KEY` for all three providers, a name no real provider
uses. The fakes accept any key (any `Authorization` header, or none). A process that serves
synthetic calls therefore never needs, and never has to be given, `DEEPGRAM_API_KEY` or
`OPENAI_COMPATIBLE_API_KEY`: if this file is ever pointed at a real host by mistake, the real
provider rejects the fake key and nothing is spent.

```sh
export FAKE_PROVIDERS_API_KEY=fake
BRISVOX_AGENT_FILE=bench/fake_providers/agent.fakes.toml BRISVOX_EXTRA_CA_FILE=$PWD/pki/ca.pem \
  ./target/release/brisvox-server                       # features all-providers,smart-turn-ort
(cd bench/pipecat_baseline && SSL_CERT_FILE=$PWD/../../pki/ca.pem \
  ../../../pcvenv/bin/python bot.py --agent ../fake_providers/agent.fakes.toml --port 8765)
./target/release/brisvox-bench v2v --url ws://127.0.0.1:8080 --fixtures en_two_utterances --runs 1   # either server
```

## What is faithful

**Deepgram live STT** (`docs/providers/deepgram.md`; the messages brisvox's assembler and
Pipecat's `DeepgramSTTService` consume):

- binary frames are linear16 audio at the `sample_rate` of the query; `KeepAlive` is accepted;
  the query parameters `endpointing`, `interim_results`, `sample_rate` are honoured;
- `Results` messages in Deepgram's shape (`channel_index`, `duration`, `start`, `is_final`,
  `speech_final`, `from_finalize`, `channel.alternatives[0]` with `transcript`, `confidence` and a
  `words` array with timings and `punctuated_word`, `metadata` with `request_id` and `model_info`):
  0.5 to 2.5 KB each, like the real ones;
- the transcript is the fixture's next utterance (the JSON sidecar is the ground truth), as the
  in-process mocks do: growing word prefixes as interims every 100 ms from the first loud audio
  (energy on 32 ms windows, RMS >= 500, the mocks' threshold), then
  - **endpointing**: after `endpointing` ms of silence (rounded up to 32 ms windows) the final
    follows 150 ms later with `is_final` and `speech_final`;
  - **Finalize**: the final follows 150 ms after the message with `is_final` and `from_finalize`
    (without `speech_final`; brisvox and Pipecat treat the answer as the end of the utterance);
    a `Finalize` with nothing pending is answered at once with an empty `from_finalize` result,
    as Deepgram does (Pipecat sends one on every VAD stop);
  - an endpoint and a `Finalize` for the same speech produce one final;
- `CloseStream` flushes what is pending, sends `Metadata` and closes; idle sockets are closed
  after 60 s.

**OpenAI-compatible chat** (`docs/providers/openai-compatible.md`): `stream: true` answers with
`chat.completion.chunk` events (`id`, `object`, `created`, `model`, `service_tier`,
`system_fingerprint`, `choices[0].delta`, `obfuscation` padding), a role chunk, the first token
350 ms after the request, a token per word or punctuation mark 30 ms apart (`Okay`, `,`, ` got`,
` it`, `.`), a `finish_reason: "stop"` chunk, a usage chunk when
`stream_options.include_usage` is set (both clients set it), `data: [DONE]`. The reply is chosen
like the mocks choose it: the last user message is matched against the fixture's utterances and
answered `Okay, got it.` / `Peki, anladım.` (anything unmatched gets the English one).
`stream: false`, `HEAD` (brisvox's `warm`) and `GET /v1/models` work. The TLS port advertises
`SETTINGS_MAX_CONCURRENT_STREAMS` = 100 like API gateways do (`--h2-max-streams`), so a client
that multiplexes more than that onto one connection queues, as it would in production
(`docs/RISKS.md` R16).

**Deepgram Aura TTS** (`docs/providers/deepgram-aura.md`): `Metadata` on open; `Speak {text}`,
`Flush`, `Clear`, `Close`; binary linear16 frames; `Flushed {sequence_id}` after the audio of all
text before the flush; `Cleared {sequence_id}` for `Clear`. First audio 150 ms after text reaches
an idle connection; 60 ms of audio per character in 40 ms frames (1920 bytes at 24 kHz) one every
20 ms (twice real time, like the mock TTS: the client's pacing, not the server's, sets what the
caller hears); text that arrives while audio is scheduled continues it. The audio is a square wave
of +-1000, audible to the benchmark client, never silence. `Clear` drops the unsent audio at once
(an ideal server; real Aura may send a little more before `Cleared`).

All latencies are flags (`--stt-final-ms`, `--llm-first-token-ms`, `--tts-first-audio-ms`, ...;
`--help`), defaults are the in-process mocks' (`EngineTargetConfig::default()`), so
"realistic-mock" numbers compare with the existing mock numbers.

## What is simplified

- The STT does not recognise anything: it reports the fixture's utterances in order (a third
  utterance on one connection gets no transcript) at times derived from the audio's energy. A
  client that sends other audio gets the fixture's text anyway. `--fixture` selects one sidecar
  per server (`en_two_utterances` by default); one server serves one script.
- No `UtteranceEnd`, `SpeechStarted`, `vad_events`, keyterms, languages, diarisation, mu-law input
  (linear16 only), `Warning`s, flush rate limit or authentication errors.
- The LLM never calls tools and has no reasoning or provider routing fields (the request body is
  parsed for `messages`, `stream`, `stream_options` and `model`, the rest ignored).
- The TTS does not check the voice (`model`) or the encoding (treated as 16-bit samples).
- One CA and one certificate with a fixed name list; no mutual TLS, no OCSP, no session
  tickets tuning beyond rustls' defaults. TLS 1.3 and 1.2.
- Faults (resets, stalls, 5xx) are not injected; the in-process mocks and `brisvox-sim` cover
  those.

## Wire check, smoke test and self-load test

```sh
bench/fake_providers/smoke.sh        # fakes + wire check + one brisvox call + one Pipecat call
python bench/fake_providers/check_wire.py --url https://127.0.0.1:8443 --ca pki/ca.pem
./target/release/brisvox-fake-providers selftest --url wss://127.0.0.1:8443 --ca pki/ca.pem --calls 1000 --seconds 30
```

- `check_wire.py` talks to the fakes with plain Python clients (websockets, httpx) and checks the
  promises above: transcripts, interims, endpointing and `Finalize` timing, `Flushed`/`Cleared`,
  audio sizes, SSE content and pacing (tolerances of 100 to 300 ms: it is a smoke check, not a
  stopwatch).
- `smoke.sh` starts the fakes, runs `check_wire.py`, then one `brisvox-bench v2v` call
  (fixture `en_two_utterances`, both turns) against brisvox-server and one against the Pipecat
  bot, both on `agent.fakes.toml` through TLS, and prints what the fakes saw. Variables:
  `BIN_DIR`, `PCPY` (the Pipecat venv's Python), `WORK`, `BRISVOX_MODELS_DIR`, `SKIP_PIPECAT=1`.
  It needs `models/` (`scripts/models/fetch_models.py`), the Pipecat venv and binaries built with
  `cargo build -p brisvox-server -p brisvox-bench -p brisvox-fake-providers --features
  brisvox-server/all-providers,brisvox-server/smart-turn-ort`. **Several worktrees sharing one
  `CARGO_TARGET_DIR` overwrite each other's `target/*/brisvox-server`**: copy the three binaries
  out of the target directory right after building and pass that directory as `BIN_DIR`.
- `selftest` is a load generator for the fakes themselves: calls of one STT socket fed in real
  time (3 s of speech, 1.5 s of silence, twice), one TTS socket and, on a plain URL, a chat request
  per turn. It prints how late the fakes are against their own promises. Run it from another
  process (or another container); read the server's CPU and memory from `/metrics`.

## Containers and Railway

```sh
docker build -f bench/fake_providers/Dockerfile -t brisvox/fake-providers .
docker run --rm -p 8443:8443 -p 8080:8080 brisvox/fake-providers
```

The image is `debian:trixie-slim` with the stripped binary and the fixture sidecars, running as a
non-root user; arguments after the image name are the server's
flags. It listens on 8443 (TLS) and 8080 (plain, or `$PORT`). The Dockerfile was written for this
task and has not been built on the Windows machine of the first runs (no Docker there).

For a deployment next to the systems under test:

1. Give it a name the others can resolve and put that name in the certificate:
   `--san fakes.railway.internal` (Railway private networking) or `--san fakes`.
2. The brisvox and Pipecat containers fetch the CA at start, before the server starts:
   `curl -fsS http://fakes.railway.internal:8080/ca.pem -o /tmp/ca.pem`, then
   `BRISVOX_EXTRA_CA_FILE=/tmp/ca.pem` / `SSL_CERT_FILE=/tmp/ca.pem`. A restart of the fakes
   makes a new CA unless `--ca-dir` is on a volume; restart the clients then.
3. Point the agent file at the name (`sed` above) and give the server processes
   `FAKE_PROVIDERS_API_KEY` only.
4. The fakes are not in the budget of the systems under test: run them as their own service and
   check `process_cpu_seconds_total` on `/metrics` so that they are not the bottleneck.

## Capacity of the fakes

`selftest`, release build, Windows 11 desktop (32 hardware threads, client and server on the same
machine), server unpinned, 30 s runs:

| calls | transport | server CPU (cores over the run) | server RSS | fakes' latency against their promise |
|---:|---|---:|---:|---|
| 1000 (2000 websockets, 50 audio frames/s each in) | plain | 1.5 to 2 | 107 MB | STT final 477 ms after the end of speech (p99 500), TTS first audio 156 ms (p99 171), Flushed 556 ms, LLM first byte 359 ms |
| 1000 | TLS | 1.5 to 2 (6000 handshakes in the run) | 108 MB | STT final 477 ms (p99 502), TTS first audio 157 ms (p99 173), Flushed 557 ms |

Windows costs more per syscall than Linux; the Linux numbers come with the container runs. A first
version re-armed a timer for every audio frame and used twice the CPU for the same load (93 CPU
seconds instead of 45 in a 30 s run), hence one timer per call that moves only when the next
deadline changes.
