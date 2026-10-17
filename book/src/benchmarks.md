# Benchmark results

Every figure on this page comes from a results file committed under `bench/results/` (the file
starts with the command that produced it and a description of the machine and setup) or from
[Latency design and measurements](benchmarks/latency.md). Regenerate a file with the command at
its top.

## Read this first

- **All numbers below are from a Windows x86_64 development machine (32 logical cores, release
  build, shared with other workloads), over mock providers** with fixed, configured latencies
  (STT final 150 ms, LLM first token 350 ms, TTS first byte 150 ms). They show how much time the
  *engine* adds on top of providers. They do not show what a live call over real providers
  feels like.
- **Linux numbers exist for two things**: the load simulation confined to 4 cores (WSL2, kernel
  6.6, Docker, over mocks, 2026-10-13; `sim-baseline-linux.md`: 500 calls, overhead p99 0.185 ms;
  the saturation sweep passes 10 ms of wake p99 at 10000 calls) and the capacity comparison below.
  The rest of this chapter is Windows: timers tick at about 15.6 ms and coalesce wake-ups, so
  those files say that figures for comparisons "come from a Linux run".
- **Capacity: the real provider path and the real models, on cloud hardware.** Every call went
  through each system's own provider client code (brisvox's Rust adapters; Pipecat's Deepgram,
  OpenAI and Deepgram Aura services) over TLS to protocol-faithful fake providers with fixed
  latencies (`bench/fake_providers`), with the Silero VAD and Smart Turn on every call, in Linux
  containers on AMD EPYC 9655P, one system at a time, 3 runs each on a fresh container
  (2026-10-16/17, `bench/results/capacity-*-cloud-*`). Within the SLO (p95 voice-to-voice at most
  1110 ms, the providers' 860 ms plus 250 ms): 4 vCPU / 4 GB **250** calls for brisvox (260, 250,
  220) against **1** for Pipecat 1.12.0 (1, 1, 0); 8 vCPU / 8 GB **470** (480, 460, 470) against
  **10** (10, 5, 10). Without a failed call or a lost turn, at any latency: **430** against **10**,
  and **780** against **10**. Pipecat is memory-bound in 4 GB (workers killed at 20 calls) and at
  8 GB lost turns at 20 calls in two of three runs; brisvox is CPU-bound and scaled about 1.9 times
  with twice the container. Memory at the maximum (PSS summed over the container's processes):
  753 MB at 430 calls against 2.4 GB at 10 (4 vCPU / 4 GB). brisvox's admission control was off
  for the ramp. The same run on local Docker: `PROVIDERS=fakes MODELS=1 bench/capacity_linux.sh`
  (`docs/LATENCY.md`, "Capacity with the real provider path").
- **Earlier comparisons over in-process mocks** (desktop, 2026-10-13/14, kept in `docs/LATENCY.md`
  "Capacity benchmark, Linux" and "Capacity with the models", including LiveKit Agents) measured
  the frameworks without the provider clients; the cloud run above replaces them.
- **Live providers.** The same agent file on both systems: Deepgram nova-3, OpenRouter
  `openai/gpt-6-luna` with reasoning effort `none` pinned to one upstream, Deepgram Aura voice; 5
  concurrent calls, 100 turns per system in strictly alternating blocks on one Windows machine over
  the public internet. Voice-to-voice p50 / p90 / p95 / p99: brisvox 1.85 / 2.21 / 2.88 / 5.20 s,
  Pipecat 1.97 / 2.98 / 4.35 / 6.14 s. Pipecat sent more than one LLM request in 56 of 100 turns
  (cause not investigated), and the same nominal VAD stop is 96 ms in Pipecat and 128 ms in brisvox
  in this setup (`bench/results/v2v-live-*-c5-*`, `docs/LATENCY.md` "Live comparison with real
  providers", which also keeps the earlier one-call 30-turn run). Other
  providers' adapters are tested against mocks and recorded protocol fixtures until their keys are
  available.

## Definitions

*Voice-to-voice (V2V)*: the first non-silent bot audio sample leaving the transport minus the
ground-truth end of the caller's speech, measured on the client side. *Engine overhead*: V2V
minus the time spent waiting on providers and on the configured end-of-turn policy. Full
definitions: [Latency design and measurements](benchmarks/latency.md).

## Engine overhead per stage

From `bench/results/v2v-sim.md` (`brisvox-bench v2v --sim --runs 10`: the production pipeline
over mock providers behind the real websocket transport, 60 latency turns and 30 barge-ins). The
engine's own timeline stamps each span where the work happens:

| Stage | p50 | p95 | Budget at p95 |
|---|---:|---:|---|
| end of turn to LLM request (`turn_end_to_llm_request`) | 0.051 ms | 0.089 ms | 3 ms, met |
| first TTS audio to transport (`tts_audio_to_transport`) | 0.035 ms | 0.064 ms | 2 ms, met |
| barge-in decision to `Clear` applied (`interrupt_to_clear`) | 0.012 ms | 0.022 ms | 1 ms, met |

The worst barge-in clear of the 30 was 0.669 ms, one scheduling delay.

## Voice-to-voice over mocks

Same run, measured by the client over the websocket: 60 of 60 turns answered, none talked over,
none missed.

| scope | turns | p50 | p90 | p95 | p99 |
|---|---:|---:|---:|---:|---:|
| all fixtures | 60 | 1009.6 ms | 1030.2 ms | 1034.3 ms | 1057.5 ms |

The configured floor in that run is 950 ms (450 ms from the end of speech to the end-of-turn
decision, plus the mock LLM's 350 ms and the mock TTS's 150 ms). What is above the floor is the
engine, the websocket transport, the 20 ms frame grid, and the Windows timer's overshoot on the
mocks' sleeps (8.4 ms at p50 in each of the LLM and TTS spans, according to
[Latency design and measurements](benchmarks/latency.md)). The engine's own stages add about 1 ms
of it; the stage table above is the engine's share.

## Memory

From the same run (resident set of the whole process, engine, transport and harness together):
10.2 MiB idle and cold, 22.1 MiB idle after the calls (allocator caches and lazily built state),
29.7 MiB peak with 8 calls alive at once: **about 1.0 MiB per call** on the production pipeline,
of which roughly half is the Smart Turn audio ring and its analysis snapshot (256 KB each).

## Concurrent calls (reference pipeline)

From `bench/results/sim-baseline.md` (`brisvox-bench sim-load --workers 4`): N simulated calls at
once in one process, real time, the reference pipeline with an energy VAD (not the production
pipeline, and not using the inference pool), 3 turns per call.

| calls | answered | failed | overhead p50 | overhead p99 | overhead max | CPU cores | peak RSS | RSS per call |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 | 300/300 | 0 | 0.077 ms | 0.842 ms | 1.231 ms | 0.28 | 22.4 MiB | 142 KiB |
| 300 | 900/900 | 0 | 0.130 ms | 1.965 ms | 2.321 ms | 0.47 | 48.8 MiB | 133 KiB |
| 500 | 1500/1500 | 0 | 0.173 ms | 1.675 ms | 6.038 ms | 0.37 | 75.8 MiB | 132 KiB |

Not a capacity limit: the host has 32 cores and the process used under half a core, so this
shows low overhead and flat memory growth, not the maximum number of calls on a given machine.
Mocks do no TLS, WebSocket, JSON or codec work. The Linux run confined to 4 cores and pushed
towards saturation is in `docs/LATENCY.md` ("Linux, confined to 4 cores").

## Speculative LLM start

From `bench/results/speculation-fixtures.md` (`brisvox-bench speculation --seeds 20`): the
production engine over mock providers on a paused clock, deterministic per seed, real FLEURS
speech fixtures, server-side V2V, the adaptive turn policy, a 350 ms mock LLM.

| speculation | turns | V2V p50 | extra LLM requests per turn |
|---|---:|---:|---:|
| off | 120/120 | 923 ms | 0.00 |
| on, STT interims that already hold the full text | 120/120 | 782 ms | 0.00 |
| on, STT interims a word short of the final | 120/120 | 925 ms | 1.00 |

Speculation helps only when the last interim transcript at the VAD stop already equals the final
one; otherwise it costs a full extra request per turn and gains nothing. The file also holds the
other turn policies, a 100 ms LLM, the stable-partial gate and the resumed-speech scenario. How
well a real STT's interims converge is a live measurement that has not been made.

## All results files

| File | What |
|---|---|
| `v2v-sim.md`, `v2v-sim.json` | V2V and engine stage times over the websocket, mock providers |
| `v2v-mock-bot.md`, `v2v-mock-bot.json` | The harness against a mock bot: harness and transport self-check |
| `sim-baseline.md`, `sim-500-1worker.md` | In-process load, N simulated calls |
| `fixtures-sim.md` | Whole simulated calls on real speech, server-side |
| `speculation-fixtures.md`, `v2v-speculation.md`, `v2v-speculation-gate.md` | Speculative LLM start |
| `inference-pool.md`, `resampler.md`, `smart-turn.md` | Component costs |

The harness (`crates/brisvox-bench`), its options and the metric definitions are described in
`bench/results/README.md`; the numbers are discussed section by section in
[Latency design and measurements](benchmarks/latency.md). How the engine is proven correct
(deterministic simulation, chaos, isolation and leak tests) is in
[Testing and the quality gate](benchmarks/testing.md).
