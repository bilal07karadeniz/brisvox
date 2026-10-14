# Optimization pass: what limited the capacity run, what was fixed, before and after

- date: 2026-10-13. brisvox 0.1.0, release build (`opt-level 3`, thin LTO, line-table debug info), Windows 11 x86_64, 32 logical cores, a shared desktop (other builds and apps running).
- **Windows, mock providers, not a Linux run, no real models**: the engine runs over `brisvox_sim` mock providers with the energy VAD and a fixed Smart Turn model (real feature extraction, no ONNX), behind the real websocket transport, as a child process (`brisvox-bench engine-server`) with 4 tokio workers and 2 inference pool threads. Timers, scheduling and the loopback TCP stack differ from the Linux servers the engine targets. The numbers show trends and relative changes, not capacity claims; the Linux run with equal container limits (`bench/capacity_linux.sh`) and the real-model run are still open.
- generator: `brisvox-bench capacity` (4 workers, same machine). SLO: p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost or unclean turn. Fixture `en_two_utterances`, 2 calls per slot, slots spread over 4 s.
- Limits moved by about one step between repeats near the cliff: the same 2100-call step answered every turn with p95 1295-1336 ms in three repeats and lost 1583 of 8400 turns in a fourth (about 1 000 pool rejections that second; the line was not recorded yet, and the queue was the default 1024 for 2048 calls allowed). Rows are single runs unless stated.

## How it was profiled

What this machine offers, and what was used instead:

| tool | available? | used |
|---|---|---|
| `cargo flamegraph`, `samply` (ETW sampling), Windows Performance Recorder / `xperf` | not installed; all of them sample through ETW, which needs an elevated shell, and this one is not elevated | no. Proposed as a follow-up on a Linux box (`perf`) or an elevated Windows shell |
| `tokio-console` | needs the `console-subscriber` crate in the tree (no new dependencies allowed here) | no; the same counters come from tokio's own runtime metrics (next row) |
| tokio runtime metrics | `--cfg tokio_unstable` build of `brisvox-bench` (`RUSTFLAGS="--cfg tokio_unstable" CARGO_TARGET_DIR=target-unstable cargo build --release -p brisvox-bench`) | yes: polls/s, mean poll time, busy share, parks, steals, spawns, I/O events per second, printed by `engine-server --stats-every-ms` |
| the engine's own counters | pool queue depth, wait and run time, rejections (new: per priority line), `VadStats`, runtime task and queue counts | yes: one `stats` line per second on the server's stderr (`engine-server --stats-every-ms N`, `capacity --sim --stats-every-ms N`) |
| per-thread CPU (user / kernel) | PowerShell `Get-Process ... .Threads` sampled twice 8 s apart at steady state | yes: tokio workers vs pool threads, kernel share |
| ablations | the bench's flags | yes: `--engine-turn fixed:300` (no Smart Turn), `--server-workers 8`, a VAD run inline instead of on the pool (temporary patch, not kept) |

## What limited the capacity run

Step 1500 (2 pool threads, the production queue of 1024 jobs, 2048 calls allowed) lost 1054-1093 of 6000 turns ("no response") in every repeat. The engine-side counters at that step (the reproduction is `capacity-brisvox-t083-before-windows-2026-10-13.md`):

| quantity (steady state, 1500 calls) | idle phases | the phases where calls answer |
|---|---|---|
| pool jobs per second | 65-75 000 (one VAD batch per call and 20 ms frame) | the same, plus 370-480 Smart Turn analyses per second |
| pool busy share | 3-5 % | 30-43 % |
| run time per job | 1.0-1.2 us | 9-11 us on average: the mix of 75 000 x 1 us VAD batches and 400 x 1.5 ms analyses (feature extraction; mean 1.4-1.7 ms, worst 3-5 ms) |
| queue wait per job | 4-5 us | 0.5-4.4 ms on average |
| queue depth | 0-1 | up to the capacity of 1024; 5 600-6 200 jobs rejected in single seconds |

**The pool was not saturated; the shared FIFO queue was.** A Smart Turn analysis occupies a pool thread for 1.5 ms; while both threads run one, the next 75 VAD batches per millisecond queue up behind them. When the turns of a few hundred calls end together (the benchmark's calls play the same fixture), the analyses arrive in bursts of several hundred, and the VAD batches of all calls wait behind them and overflow the queue. A rejected VAD batch loses the call's VAD stream (`Failure::Job`: the stream and the batch go with the job), which closes the speech the stream knew about: the turn is lost, and the extra speech stops start more analyses (a feedback loop; one second of the run saw 944-1 274 analyses against the usual 370).

Evidence that Smart Turn jobs are the cause: the same step with `--engine-turn fixed:300` (no analyses, `capacity-brisvox-t083-ablation-fixed-turn-windows-2026-10-13.md`) passes, 6000 of 6000 turns, p95 1057 ms, with the pool 3-4 % busy and a queue depth that never exceeded 310.

What the engine side looks like when the pool is not the limit (thread CPU, 1500 and 2100 calls, after the fix):

| | tokio workers (4) | pool threads (2) |
|---|---|---|
| 1500 calls | 64-67 % each | 33-36 % each |
| 2100 calls | 68-72 % each, 27 % of it kernel (loopback TCP send and receive) | 35-38 % each |
| tokio runtime metrics at 2100 calls | 240 000-360 000 polls/s, mean poll 9-14 us (syscalls included), busy 82 % of the 4 workers, 0 parks/s, 130 000-170 000 I/O events/s, global queue 1 000-3 700 tasks | |

That is about 1.4 polls per audio frame (stages drain their inbox per wake-up, so the hop count is not the cost) and a third of the worker time in the kernel. With the pool no longer the limit, the 4 tokio workers are.

## What was changed

1. **Two priority lines in the inference pool** (`brisvox_inference::Priority`, `PoolHandle::run_with`, `Offload::run_with`). The VAD batch of every call goes in the `High` line; everything else (Smart Turn, warm-ups, builds, normalization) stays in the `Normal` one. A worker always takes a waiting high job first and never interrupts a running one. Each line is bounded by the queue capacity on its own (reject newest, as before), so a flood of analyses cannot push a VAD batch out and the reverse. The shared `mpsc` channel behind a `Mutex<Receiver>` became a `Mutex` plus `Condvar` over two `VecDeque`s; a submitter signals only when a worker is waiting. `PoolMetrics::rejected_high` counts high-line rejections (should stay zero). Under overload the analyses are shed first (their own timeout), which is the right order of loss: the turn machine handles a missing verdict, a lost VAD window loses the call's speech state.
2. **`brisvox-server` sizes the pool queue `max(1024, 2 x max_sessions)`**: each line must hold one job per call; the default of 1024 covers 512 sessions.
3. **Timeline marks (carried over from the earlier audit)**: `TurnEvent::FirstChunk` (segmenter, once per response instead of a lock per chunk) and `TurnEvent::TtsFirstText` (TTS stage, once per response, next to `LlmFirstSentence` and `TtsRequest`); `TimelineRecorder::is_marked` lets `TransportOut` skip its `AudioOutStopped` mark when the turn driver recorded it (the second mark was counted as repeated). New V2V stage `first_chunk_to_tts_text` (the hop between the two stages: 0.007 ms p50, 0.008 ms p95).
4. **Profiling hooks kept in the harness**: `engine-server --stats-every-ms N` (and the same flag on `capacity --sim`), `EngineServer::pool_metrics`, the tokio runtime-metrics line under `--cfg tokio_unstable`.

## Capacity, before and after

SLO p95 V2V <= 1200 ms, 2 pool threads, 4 tokio workers, Windows, mock providers. "Before" is the first capacity file `capacity-brisvox-queue1024-calls2048-windows-2026-10-13.md` and this session's reproduction of step 1500 with the pre-fix binary.

| concurrent calls | before: answered / expected, p95 V2V | after: answered / expected, p95 V2V | after: target CPU |
|---:|---|---|---:|
| 1000 | 4000 / 4000, 1032 ms | 4000 / 4000, 1035 ms | 1.96 cores |
| 1200 | 4800 / 4800, 1041 ms (reproduction 1047 ms) | 4800 / 4800, 1044 ms | 2.23 cores |
| 1500 | **4904 / 6000** (reproduction 4923 / 6000), p95 1060 ms, p99 3626 ms (reproduction p95 2357 ms): FAIL | 6000 / 6000, 1057 ms (second run 1072 ms): pass | 2.76 cores |
| 1800 | not run | 7200 / 7200, 1168 ms: **pass** | 3.12 cores |
| 2000 | not run | 8000 / 8000, 1224 ms: fail on p95 only | 3.15 cores |
| 2200 | not run | 8798 / 8800, 1324 ms: fail | 2.53 cores |
| 2400 | not run | 9370 / 9600, 1398 ms, 2 calls failed to connect (Windows ephemeral ports, a harness limit): fail | 2.52 cores |
| 2000 with 8 tokio workers | | 8000 / 8000, 1068 ms: pass | 4.13 cores |
| 2200 with 8 tokio workers | | 8800 / 8800, 1103 ms: pass | 4.41 cores |

- **Largest step within the SLO: 1200 before, 1800 after** (+50 %, one run each, a step is about 200-300 calls wide). Above 1800 the engine now degrades the way a saturated server should: the p95 climbs with every turn still answered (2000), instead of losing a fifth of the turns at 1500.
- The limit above 1800 is the **4 tokio workers**: the same server with 8 workers passes 2200 (the run stopped there). The 4-worker rows above 2000 also show the generator's own limits (2 failed connects at 2400 are Windows ephemeral port exhaustion).
- CPU per call at the maximum: 3.12 cores / 1800 calls = 1.7 mcores (before at 1200: 2.08 cores / 1200 = 1.7 mcores): the fix moved the cliff, it did not make a call cheaper. Most of a call's CPU is per audio frame, two thirds user time (websocket framing, stage hops) and a third kernel (loopback send / receive).
- Files: `capacity-brisvox-priority-queue1024-calls2048-windows-2026-10-13.{md,json}` (same flags as the first capacity run), `capacity-brisvox-priority-queue6000-calls3000-windows-2026-10-13.{md,json}` (the extended ramp, queue sized 2 x calls), `capacity-brisvox-priority-workers8-windows-2026-10-13.{md,json}`, and the before / ablation files `capacity-brisvox-t083-{before,ablation-fixed-turn}-windows-2026-10-13.{md,json}`.

## V2V, before and after

`brisvox-bench v2v --sim --runs 10`, one call at a time, 60 answered turns each, the same machine, back to back.

| | p50 | p90 | p95 | p99 | max | engine stages (p95): into LLM / out of TTS / clear |
|---|---:|---:|---:|---:|---:|---|
| before (pre-fix binary) | 1009.4 ms | 1026.1 ms | 1028.0 ms | 1042.8 ms | 1042.8 ms | 0.074 / 0.055 / 0.027 ms |
| after (this change) | 1006.9 ms | 1028.8 ms | 1034.2 ms | 1042.5 ms | 1042.5 ms | 0.070 / 0.056 / 0.017 ms |

No change beyond run-to-run noise (a few ms of Windows timer jitter), as expected: at one call the pool is idle. Full reports: `v2v-sim-t083.md` / `.json` (after) and `v2v-sim-t083-before.md` / `.json`.

## What did not work

- **A burst limit instead of strict priority.** The first version of the high line let a worker take one normal job after every 8 high ones while a normal job waited. With 75 000 VAD batches a second against 400 analyses, 8 batches per 1.5 ms analysis is 5 000 batches a second per thread: the high line overflowed and the step lost half of its turns (2946 of 6000). Strict priority fixed it; starvation of the analyses is bounded by their callers' timeouts and the queue bound instead.
- **Removing the per-job timeout timer** (`clock.sleep` per pool request, 75 000 a second): target CPU at 1500 calls 2.88 / 2.71 cores with it, 2.79 / 2.82 without. Not a hotspot; not changed.
- **Running the VAD inline instead of on the pool** (temporary patch, to price the dispatch: allocations, oneshot, wake): target CPU 2.92 vs 3.19 cores at 2100 calls, tokio worker CPU unchanged at about 70 %. The pool hop is not what limits the workers.

## Remaining gains, not done

1. Linux profile (`perf` / `cargo flamegraph`) of the 4-worker server at 1800-2000 calls to split the worker time between websocket framing and the engine's stages: this machine cannot sample (no elevated shell). Candidates: per-frame work in `brisvox-transport`'s websocket read / write path, the 20 ms playout pacer timers, the `Frame` hops.
2. The VAD worker still loses the stream when its batch is rejected (`Failure::Job` drops the closure that owns the stream). With the lines sized to the call limit this needs more outstanding VAD jobs than calls, but a retry that keeps the stream (the batch stays in the tee, bounded by its drop-oldest policy) would make overload a delay instead of a lost turn.
3. Smart Turn analysis cost: 1.0-1.5 ms of mel features per analysis in the sim (22.5 ms with the ONNX model, `smart-turn.md`); an incremental spectrogram over the ring would cut the features to the new frames. Relevant with the real model only at a few percent, so not pursued here.
4. Windows-only measurement noise (timer tick, ephemeral ports at 2400 calls): the 4-worker rows above 2000 need the Linux run.
