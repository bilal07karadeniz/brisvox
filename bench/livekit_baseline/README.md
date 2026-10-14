# LiveKit Agents capacity baseline

A LiveKit Agents worker with key-free mock providers and a WebRTC load generator, so that
`brisvox-bench capacity` (brisvox), `bench/pipecat_baseline` (Pipecat) and this directory (LiveKit
Agents) answer the same question the same way: how many concurrent calls does one 4-CPU / 4 GiB
container carry with p95 voice-to-voice (V2V) latency within 1200 ms and no lost turn? The mock
STT, LLM and TTS have the latencies and shapes of the `brisvox_sim` mocks; the pipeline above them
(VAD, turn detection, preemptive generation, sentence stream adapter, audio output, WebRTC room
I/O) is LiveKit's own.

| file | what |
|---|---|
| `agent.py` | the worker: `AgentServer` + one `AgentSession` per job; the framework's defaults, with `LK_*` environment overrides for the variants |
| `lk_mock_providers.py` | mock `stt.STT`, `llm.LLM`, `tts.TTS` behind LiveKit's plugin interfaces; the pure helpers (script, answer lookup, tone) are imported from `bench/pipecat_baseline/mock_providers.py`, so there is one definition of them |
| `loadgen.py` | the load generator: N room participants (livekit rtc Python SDK) replay `fixtures/en_two_utterances` in real time; a port of the Rust ramp, V2V tracker, verdict rules and report (`brisvox-bench/capacity/2` JSON + markdown) |
| `sampler.py` | CPU and memory by cgroup (agent container, LiveKit server container, load generator container) |
| `capacity_livekit.sh` | the driver: three containers per run, a fresh set per run, waits for a quiet host |
| `livekit.yaml` | the LiveKit server config (dev key, loopback, one muxed UDP port) |
| `Dockerfile`, `requirements.txt`, `requirements.lock.txt` | the image of the worker and of the load generator (the whole dependency tree is pinned) |
| `tests/test_loadgen.py` | offline tests of the V2V tracker, gap padding, verdicts and ramp (`python -m unittest discover -s bench/livekit_baseline/tests`) |

## Result (2026-10-14, WSL2 Linux 6.6, Docker 29.8, 4 CPUs / 4 GiB, mock providers, 3 runs each)

| LiveKit Agents 1.8.7 variant | max calls within the SLO per run | median | p95 V2V at the max | agent CPU at the max | memory at the max (PSS) | files |
|---|---|---:|---|---|---|---|
| **tuned**: turn-detector threshold lowered, `load_threshold` 1.0 | 22, 23, 25 | **23** | 1171, 1171, 1192 ms | 2.09, 2.08, 2.22 cores (about 90 mcores per call) | 2.9, 3.0, 3.3 GB (cgroup 3.0-3.4 GB) | `capacity-livekit-models-linux-2026-10-14*` |
| **defaults**: the framework as shipped | none (0, 0, 0) | 0 | p95 2.81-2.89 s already at 1 call | 0.13 cores at 1 call | 0.6 GB at 1 call | `capacity-livekit-default-models-linux-2026-10-14*` |

The file without a `-run<N>` suffix is the summary of its three runs (`bench/capacity_summary.py`).
**The tuned variant carries the name `capacity-livekit-models-...`** because that is the file the
chart script reads, like Pipecat's tuned run; the default result is kept next to it and explained
below, not dropped. For context, the same SLO, container and mocks gave brisvox 200-325 and Pipecat
1.12 (tuned, with its models) 20 calls (`docs/LATENCY.md`, "Capacity with the models").

Per step of the tuned variant (run 1; runs 2 and 3 within a few percent):

| calls | p50 / p95 V2V | agent CPU | memory (PSS) | processes | LiveKit server CPU / RSS |
|---:|---|---:|---:|---:|---|
| 1 | 1137 / 1159 ms | 0.14 | 654 MB | 10 | 0.03 cores / 70 MB |
| 5 | 1130 / 1146 | 0.52 | 1064 | 17 | 0.11 / 101 |
| 10 | 1135 / 1158 | 1.01 | 1615 | 27 | 0.20 / 153 |
| 15 | 1132 / 1155 | 1.41 | 2165 | 37 | 0.29 / 193 |
| 20 | 1139 / 1180 | 1.82 | 2713 | 47 | 0.38 / 245 |
| 22 | 1140 / 1170 | 2.09 | 2935 | 51 | 0.44 / 276 |

- **The LiveKit server (SFU) is reported separately and is not in the 4-CPU budget.** At the
  maximum it used 0.44-0.47 cores and 276-294 MB RSS (about 20 mcores and 7 MB per call), in its
  own container on cores 12-15, no limit (`capacity-livekit-sfu-2026-10-14.md`).
- **The load generator used 2.3-2.5 cores** (8 Python processes on cores 4-11) at the maximum,
  against 0.1-1.7 cores for the Rust generator; no step was generator limited (1 late frame of
  about 30 000 over 100 ms at worst; largest send lateness 105 ms, usually 30-60 ms).
- A call costs about 90 mcores and 105-130 MB: more than brisvox, in the range of Pipecat with
  its models for CPU, and well below it for memory (about 130 MB per call, baseline included,
  against about 200 MB for Pipecat with its models, both PSS; LiveKit shares the preloaded model
  pages between the job processes, which is why a plain RSS sum is not used, see below).
- The one-call V2V is **1130-1140 ms (p50)**: 180 ms above the 950 ms floor of the mocks, where
  brisvox is at 985 ms and Pipecat at 1008 ms with the energy VAD. Most of the 180 ms is the
  WebRTC path, not the framework (see "Differences"). So only 60-70 ms of the 250 ms SLO
  allowance is left at one call, and the capacity found here is where p95 crosses 1200 ms or
  calls are refused, not where CPU or memory run out: at the maximum the container uses 2.1-2.2
  of its 4 cores and 2.9-3.3 of its 4 GiB.

## Why the defaults do not pass even one call

With the session defaults, one call of two turns has a p50 of about 1.13 s on some turns and
**2.8 s on the others** (run 1: 3 of 6 turns, run 2: 3 of 6, run 3: 2 of 6; per turn data in
`capacity-livekit-default-models-linux-2026-10-14-run<N>.turns.jsonl`). The slow turns are the
turn detector holding the turn: `AgentSession` defaults to `inference.TurnDetector()`, which
outside LiveKit Cloud is the local audio model `v1-mini`. After each utterance it returns an
end-of-turn probability; below the language's `unlikely` threshold (0.36 for English) the session
waits `max_delay` (2.5 s for a streaming turn detector) instead of `min_delay` (0.3 s). A debug
trace of single calls (the agent at `--log-level debug`, "eot prediction" lines) showed the
probabilities for the two fixture utterances at 0.316 and 0.353, both just under 0.36, in one run;
in another call the same audio passed. The two utterances (FLEURS read speech, cut at the end of
the sentence) are borderline for this model. The slow turns then end 2.5 s after the end of
speech plus the mock chain, 2.8 s in V2V. With 45 % of the turns slow p95 is 2.8 s and the step
fails at one call, whatever the load.

This is a property of the model and of these clips, not of the container's capacity, and it would
make every capacity number zero, so the capacity run needs the verdict out of the way while the
model stays in the cost.

## What the tuned variant changes, and why it is fair

| setting | default | tuned | effect |
|---|---|---|---|
| `inference.TurnDetector(unlikely_threshold=...)` | calibrated per language (en 0.36) | 0.05 | the model still runs on every turn (same CPU, same memory), but its "probably not finished" verdict no longer extends the wait from `min_delay` to `max_delay` |
| `AgentServer(load_threshold=...)` | 0.7 (prod default) | 1.0 | the worker keeps accepting jobs until its CPU is used up; at 0.7 it reports itself full above 70 % of the container's CPU, and the server then sends no job to it |

Everything else is the framework default: process per job (`forkserver`), `num_idle_processes` =
the container's CPUs (4), the session's Silero VAD and endpointing (`min_delay` 0.3 s,
`max_delay` 2.5 s), preemptive generation on, interruptions on, a sentence stream adapter around
the non-streaming mock TTS. Why this is fair:

- The turn detector **runs on every turn** in the tuned variant, so its CPU and memory are part of
  the result. Only the decision no longer delays the fixture's turns. The other two systems are
  measured the same way: their turn model (Smart Turn v3.2, or the fixed "complete" of the
  no-model runs) answers "turn finished" for these fixtures, and the cost is in the numbers.
- The threshold is a documented option of `inference.TurnDetector` (the framework logs a
  warning that overriding the calibrated default "may be suboptimal"). A deployment whose
  callers pause mid-sentence needs the default; this benchmark's callers never do.
- `load_threshold` 1.0 is admission control moved out of the way, as Pipecat has none (its
  sizing guidance is autoscaling on active sessions). With the default 0.7 the worker would
  refuse jobs at about 2.8 cores; that is a legitimate production setting, not measured here
  (a `threshold` variant of `capacity_livekit.sh` is prepared and was not run).
- The default result is published next to it. Neither variant moves the SLO: p95 <= 1200 ms,
  no failed call, no lost turn.

## Methodology

Same as `docs/LATENCY.md`, "Capacity benchmark", and `bench/capacity_linux.sh`:

- fixture `en_two_utterances` (two utterances, gaps padded to 4 s), replayed at real time with
  20 ms frames; 2 calls per slot, slot starts spread over 4 s, 200 ms between a slot's calls, 3 s
  cooldown, one warm-up call; ramp 1, 5, 10, 15, 20, 25, 30, 40, 50, 60, 80 bisected to 2 calls;
  3 runs, each against a fresh set of containers.
- V2V = arrival of the first audible bot frame (peak >= 200 of 32768, 20 ms frames at 16 kHz)
  after the ground-truth end of the user's speech, minus that end of speech; turn outcomes
  answered / talked over / bot speaking / no response; `Tracker` in `loadgen.py` is a port of
  `v2v/measure.rs` (tests in `tests/`). SLO: p95 <= 950 + 250 = 1200 ms, no failed call, no turn
  lost or not clean, generator limited when more than 1 % of the frames are over 100 ms late.
- Mocks: STT interim every 100 ms from the first loud 32 ms window, final 150 ms after the
  utterance ends; LLM first token 350 ms (the whole answer in one delta); TTS first byte 150 ms,
  then a 40 ms chunk every 20 ms, 24 kHz. Floor = 300 + 150 + 350 + 150 = 950 ms.
- Containers (all `--network host`, so no NAT in the path): the worker `--cpus=4
  --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576` (the same limits as
  brisvox and Pipecat); the LiveKit server on cores 12-15 without a limit; the load generator
  on cores 4-11 (8 processes). Python 3.13 (`python:3.13-slim-bookworm`, as the Pipecat run).
- A call: the caller joins a fresh room with a token, publishes a microphone track, and the
  server dispatches the agent job (automatic dispatch, no agent name). The stream origin is
  the moment the agent is in the room, its audio is subscribed and its `lk.agent.state` is no
  longer `initializing` (the analogue of the websocket `started`); the agent greets nobody, so the
  first audio is the answer to turn one. A call that has no ready agent after 10 s fails ("no
  answer to start"; the websocket clients wait 5 s). A step waits up to 20 s after the cooldown
  until the worker is idle again (CPU below 0.15 cores), because a process-per-job worker tears down
  a process per call.
- Measurement: CPU is the container cgroup's `usage_usec` (every process the container ever ran,
  exited jobs included: reading the live processes' CPU time as `brisvox-bench` does for a
  server tree would lose each finished job's CPU). Memory in the schema's RSS column is the
  **PSS** summed over the container's processes: a LiveKit worker forks its jobs from a server
  that preloaded the models, so a plain sum of `VmRSS` counts the shared pages once per process
  (18 GB for 22 calls in a 4 GiB container; kept as `rss_sum_peak_bytes` and shown in the second
  table of every `.md`). `brisvox-bench capacity --pid` has recorded the PSS of the process tree
  too now (`pss_peak_bytes` next to the RSS sum in `rss_peak_bytes`, read from
  `smaps_rollup` as root), and Pipecat's 2026-10-14 runs carry it: LiveKit, Pipecat and brisvox
  memory are all PSS now and comparable. `cgroup_memory_peak_bytes` is `memory.current`
  (includes page cache).
- The per-run JSON is the `brisvox-bench/capacity/2` schema with extra fields per step (`sfu`
  usage, `rss_sum_peak_bytes`, `cgroup_memory_peak_bytes`); the Rust test
  `crates/brisvox-bench/tests/capacity.rs` reads them.

## Pinned versions

| what | version |
|---|---|
| `livekit-agents`, `livekit-plugins-silero`, `livekit-plugins-turn-detector` | 1.8.7 (PyPI, released 2026-10-14; the two plugins are only installed, the session uses `livekit.agents.inference`) |
| `livekit` (rtc SDK), `livekit-api` | 1.1.20, 1.2.1 |
| `livekit-local-inference` | 0.2.7 (native Silero VAD and turn-detector runtime) |
| Python | 3.13 (`python:3.13-slim-bookworm`) |
| livekit-server | `livekit/livekit-server:v1.13.9`, `sha256:d0c04791bf63ca8dcea123571827d56ba9504bd57c8d5de60ee03b5408cc3154` |
| VAD | `inference.VAD(model="silero")`, the session default: Silero v5, `min_silence_duration` 0.25 s, activation 0.5 (the `livekit-plugins-silero` plugin, with 0.55 s, is not what a default session uses in 1.8.7) |
| turn detector | `inference.TurnDetector()`, the session default; resolves to the local audio model **`v1-mini`** here. The cloud model `v1` needs LiveKit Inference. The text-based `MultilingualModel` of the plugin package is deprecated in 1.8.7 in favour of this class and was not measured. |

## Differences from the other two systems

- **WebRTC through an SFU, not a websocket.** The caller's audio is Opus-encoded, goes through
  the LiveKit server and is decoded and jitter-buffered by the agent (and the same in reverse).
  That path is part of what a LiveKit deployment pays and is part of the measured V2V. A
  single-call trace (`LK_TRACE=1`) put the agent's final transcript 587 ms after the end of
  speech, where the mocks alone give 470 ms: about 100 ms of one-way WebRTC input latency; the
  output side adds some tens of ms in the caller's jitter buffer. The websocket systems have no
  such buffers.
- **The SFU is not counted in the budget.** It runs in a container of its own (0.44-0.47 cores,
  276-294 MB at the maximum); a deployment needs it (LiveKit Cloud or a self-hosted server). The
  agent container alone is compared with the 4-CPU containers of the others.
- **A process per job.** Every call is an OS process (forked from a preloaded server, 4 idle ones
  kept ready), so a call starts with a process dispatch and ends with a process exit; that is
  LiveKit's isolation model and a reason for the memory per call.
- **The mock STT detects the end of the utterance itself** (10 quiet 32 ms windows, as the VAD
  stop of the other mocks) because LiveKit gives a streaming STT no end-of-speech signal (a
  provider endpoints on its side). The final arrives at the same time as in the other two.
- **The load generator is Python.** It paces 50 frames per second per call in 8 processes; its
  CPU is not in the result, its lateness is measured and judged like the Rust generator's.

## Threats to validity

- **Every failure of the sweeps was a second-round call.** All failed calls of the five steps
  that had any (23 and 25 calls in run 1, 25 in run 2, 27 and 30 in run 3) were the second call of a slot, started 200 ms after the
  first ended; each failed with "no agent ready after 10 s", while the calls that succeeded had
  their agent in 0.2-0.8 s. At that moment all N first-round jobs end within the same 4 s
  and N new rooms start: a burst of process teardown and job starts. Probable cause (not
  verified; the worker's status messages were not logged): the burst saturates the 4-core
  quota, the worker's 2.5 s load average reaches the threshold, and the server sends no job to a
  full worker. Resource use at those steps was below that of the preceding passing step
  (2.0 of 4 cores, 2.8-3.1 GB), so it is not a steady-state limit. A diagnostic run later
  (INFO logging, steps 1 to 25 in one worker lifetime) failed at 25 only on p95 (1205 ms) and
  assigned all 153 jobs; the failures therefore also depend on timing and host load. The
  protocol is the one of the other systems, which have no process spawn per call; with calls
  that arrive spread out over time, LiveKit would probably carry more than 23.
- **p95 sits close to the limit.** The one-call p95 is 1133-1163 ms; at 20-25 calls it is
  1170-1192 ms. The capacity is where an extra 40 ms tips p95 over 1200 ms, so a small change of
  the SLO moves it (the SLO was kept at the other systems' 1200 ms; an SLO of the measured one-call
  floor plus 250 ms was considered and not run).
- **Host noise.** WSL2 shares its CPUs with Windows and with other work on this machine. The driver
  waited for a Windows CPU load of at most 20 % (6 samples over 12 s) before each run: 8, 12 and
  3 % before the tuned runs, 4, 2 and 6 % before the defaults; the load during a run was not
  recorded. The earlier runs of the other systems were on a quiet host as well, but not
  interleaved with these.
- **UDP buffer.** The LiveKit server warns that the kernel's UDP receive buffer
  (`net.core.rmem_max`, 425 984) is below its recommended 5 000 000. It was not changed (a
  host-wide setting), so packet loss in the SFU cannot be excluded; no step showed lost audio
  (no `no_response` or dropped turn without a failed call).
- **Mocks, not providers.** No TLS, no provider SDK work, no real network; a real STT/TTS adds
  per-call work to the worker (the websocket or HTTP clients), as for the other systems.
- **One fixture, two turns, 4 s spread.** Same as the others; the borderline verdicts of the
  turn detector are specific to these two clips.
- **Single machine, WSL2.** Ratios between systems are more trustworthy than absolute numbers;
  native Linux would differ.

## Reproduction

```sh
# in WSL2 / Linux with Docker; from the repository root
docker build -t brisvox-livekit:1.8.7 bench/livekit_baseline
VARIANT=tuned   RUNS=3 bench/livekit_baseline/capacity_livekit.sh   # capacity-livekit-models-linux-<date>-run<N>.*
VARIANT=default RUNS=3 bench/livekit_baseline/capacity_livekit.sh   # capacity-livekit-default-models-linux-<date>-run<N>.*
python3 bench/capacity_summary.py bench/results <date>              # the summaries without -run<N>
```

`STEPS`, `RESOLUTION`, `RUNS`, `FLOOR_MS`, `CPUS`, `MEM`, `QUIET_PCT` are environment variables of
the script. Per-run `.turns.jsonl` has every call's turns and its time to a ready agent;
`.agent.log` has the worker's warnings (the shutdown noise of closed data channels and the
turn-detector threshold warning are filtered out). The agent can be run by hand:
`LIVEKIT_URL=... LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... python agent.py start`
(`LK_TRACE=1` prints session events with wall-clock times; the other `LK_*` variables are listed
in `agent.py`).
