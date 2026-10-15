# Listening test: brisvox and Pipecat under load, on a PaaS

Capacity numbers say where latency crosses a line; this test lets a person **hear** it. Each
system runs in its own container with the same CPU and memory limit (4 vCPU, 4 GB). A load
generator keeps N synthetic calls running in each container through fake providers, and meanwhile
you make one real call from your browser into the same container, with real providers. Raise N and
listen for the point where replies get slow, choppy or lost.

```
                         private network                                 public HTTPS
 loadgen ── N synthetic calls (ws) ──> brisvox container ─┬─ load process ──> fakes (TLS)
  (control page,                        4 vCPU / 4 GB     └─ real process <── your browser
   real-call button) ── N synthetic calls ──> Pipecat container ─┬─ load process ──> fakes (TLS)
                                             4 vCPU / 4 GB       └─ real process <── your browser
```

| service | what runs | public |
|---|---|---|
| `fakes` | `brisvox-fake-providers` (Deepgram live STT, OpenAI-compatible chat, Deepgram Aura over TLS; `bench/fake_providers`) | no |
| `brisvox` | one `brisvox-server` serving two agents, and `brisvox-bench container-stats` | the WebRTC listener's `/ws` |
| `pipecat` | two Pipecat bots (`bench/pipecat_baseline/bot.py`) and `brisvox-bench container-stats` | the real process |
| `loadgen` | `brisvox-bench hold`: the synthetic callers and the control page | the page |

## What each container runs

**brisvox: one process, two agents.** The owner's call and the synthetic calls share one
inference pool, one Smart Turn executor and one admission control, so the real call hears what a
real caller would at that load:

- the **public port** is the WebRTC signaling listener; the browser's call uses its WebSocket
  fallback (`/ws`; the PaaS forwards no UDP, so the media socket stays on loopback) and runs the
  live comparison agent (`bench/pipecat_baseline/agent_live_en_aura.toml`: Deepgram nova-3, an
  OpenRouter model with reasoning off and a pinned upstream, Deepgram Aura
  `aura-2-thalia-en`). It accepts only `CALL_TOKEN`, from the control page's origin
  (`BRISVOX_WEBRTC_CORS_ORIGIN`);
- the **private port 9000** is the generic websocket listener; it runs its own agent
  (`BRISVOX_WS_AGENT_FILE`, `docs/OPERATIONS.md`): `bench/fake_providers/agent.fakes.toml`
  pointed at the fakes. The synthetic calls go through each system's real provider client code
  (TLS, wire formats, the real Silero VAD and Smart Turn models) at no API cost. Its key variable
  is `FAKE_PROVIDERS_API_KEY`, which no real provider accepts, and the load generator holds only
  `LOAD_TOKEN`, which that listener alone accepts.

**Pipecat: two processes.** Pipecat runs one agent per process, so the container runs a load
process (4 uvicorn workers, one event loop per vCPU as in the capacity benchmark, started with an
empty environment so it never sees a key) and a real process (one worker, `CALL_TOKEN`). Both
share the container's CPU and memory, not their event loops.

An earlier brisvox layout used two processes too; with the fakes unreachable, its load process
held 0 provider key variables and a synthetic call got only failed provider connections
(`stt connect failed ... disconnected`, then the circuit breaker opened).

- **container-stats** (`:9100`, private): the container's CPU (cores used over the last 2 s),
  memory from cgroup v2, and the CPU model and VNNI flags, for the page.

Both real paths speak the brisvox WebSocket protocol (`docs/WEBSOCKET_PROTOCOL.md`; the Pipecat
baseline implements it), so one browser module (`sdk/web/brisvox-call.js`, transport
`websocket`) calls either one the same way.

## The control page

`https://<loadgen domain>/?key=<HOLD_ACCESS_KEY>` (the key is kept for the tab and removed from the
address bar). Per system:

- **CPU and memory** of the container (both processes together);
- **synthetic load**: preset N buttons or any N; new calls start 20 ms apart; each synthetic caller
  plays `fixtures/en_two_utterances` (two questions), waits for both answers, hangs up and dials
  again;
- **synthetic callers, last 30 s**: V2V p50/p95/max (end of the caller's speech to the first audible
  reply, measured by the load generator), answered and missed turns, failed calls, and dials the
  server refused with 503 (brisvox refuses new calls when its inference pool is under pressure
  instead of degrading the calls it has);
- **your call**: the button opens the real call; the meters show your microphone and the bot; "your
  last replies" estimates the reply time from the levels in the browser (end of your speech to
  the first bot audio; approximate, it includes the network).

## Build and deploy

Linux binaries (x86_64, glibc of Debian trixie): `brisvox-server` with
`all-providers,smart-turn-ort,webrtc`, `brisvox-bench`, `brisvox-fake-providers`; models from
`scripts/models/fetch_models.py`. Then lay out one build context per service and deploy each:

```sh
cargo build --release -p brisvox-server --features all-providers,smart-turn-ort,webrtc \
  -p brisvox-bench -p brisvox-fake-providers
BIN=target/release OUT=/tmp/railway-stage bench/railway/stage.sh
for s in fakes brisvox pipecat loadgen; do
  railway up /tmp/railway-stage/$s --path-as-root --service $s --detach
done
```

Service settings on Railway:

| service | variables | other |
|---|---|---|
| `fakes` | `PORT=8080` | no domain |
| `brisvox`, `pipecat` | `DEEPGRAM_API_KEY`, `OPENAI_COMPATIBLE_API_KEY`, `CALL_TOKEN`, `PORT=8080`; brisvox also `LOAD_TOKEN` and `LOADGEN_PUBLIC=${{loadgen.RAILWAY_PUBLIC_DOMAIN}}` | limits 4 vCPU / 4 GB; domain on 8080 |
| `loadgen` | `LOAD_TOKEN`, `CALL_TOKEN`, `HOLD_ACCESS_KEY`, `PORT=8080`, `BRISVOX_PUBLIC=${{brisvox.RAILWAY_PUBLIC_DOMAIN}}`, `PIPECAT_PUBLIC=${{pipecat.RAILWAY_PUBLIC_DOMAIN}}` | domain on 8080 |

The services find each other as `<name>.railway.internal` (private network; every listener binds
`[::]`). The brisvox and Pipecat containers fetch the fakes' throwaway CA from
`http://fakes.railway.internal:8080/ca.pem` at start and trust it for the load process only
(`BRISVOX_EXTRA_CA_FILE`, `SSL_CERT_FILE`). Restarting `fakes` makes a new CA: restart `brisvox`
and `pipecat` after it.

The same four images run on any Docker host: give the containers the network aliases
`fakes.railway.internal`, `brisvox.railway.internal` and `pipecat.railway.internal`, limit the two
servers with `--cpus 4 --memory 4g`, and publish the loadgen's port.

## Reading the result

- The synthetic V2V of both systems at light load is the floor of the fakes (STT final, LLM first
  token, TTS first audio) plus each system's own overhead; the difference between the systems is
  the part that matters.
- A container whose CPU is at its limit delays every call in it, your real call included: that is
  the point of putting your call in the same container.
- Memory at the limit means swapping or the out-of-memory killer: watch the memory bar while N
  grows.
- The load generator's own numbers are only valid while it keeps up: `late_frames` in
  `/api/stats` counts audio frames it sent more than 100 ms late (should stay 0).
