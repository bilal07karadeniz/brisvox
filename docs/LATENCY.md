# Latency

## Definitions (used by every benchmark; do not change silently)
- **User end-of-speech (EOS)**: ground-truth timestamp from fixture sidecar.
- **Voice-to-voice (V2V)**: first non-silent bot audio sample leaving the transport minus EOS,
  measured on the client side of the transport.
- **Internal overhead**: V2V minus time spent waiting on providers and on the configured
  end-of-turn policy. Measured in simulation with mock providers.

## Turn timeline
Code: `brisvox_core::timeline`. Each stage calls `TimelineRecorder::mark(epoch, event, clock.now())`
with the epoch of the frame/turn it is working on (never a guessed future epoch). Epochs are turn
numbers: the turn state machine starts a new epoch for every user turn. Rules:
- Mark rule per event: first occurrence wins, except `UserSpeechEnd`, `VadStop` and `SttFinalize`
  (latest before `TurnEnd`) and `SttFinal` (latest before `LlmRequest`): a user may pause and
  resume within one turn.
- Stages that do not know turn epochs (the engine's VAD worker and STT stage) mark the user turn
  in progress with `mark_open_turn(event, at)`: ignored (counted late) when there is no turn yet or
  the current turn has already ended.
- A newer epoch finishes the current turn (summary event + history); an older or already finished
  epoch is *late*: counted, never attributed elsewhere. Exception: `AudioOutStopped` (it happens
  after its turn) patches the finished record of its epoch.
- Barge-in: the turn state machine calls `interrupt(old, new, at, speech_start)`, which marks
  `Interrupted` on `old` and starts `new` atomically (seeding its `UserSpeechStart` with the
  barge-in speech start), so a stage that tags the new turn first cannot lose it.
- Session end must call `finish()`; summaries are otherwise emitted when the next turn starts.
- Timestamps come from the injected `Clock`; a metric whose endpoints are missing or reversed is
  `None`, never negative. VADs backdate `UserSpeechEnd` to the last speech frame (the fixtures' ground
  truth is compared against that).

| Event | Marked by | Exact moment |
|---|---|---|
| `UserSpeechStart` | VAD | first VAD speech-start of the turn |
| `UserSpeechEnd` | VAD | last speech frame before the turn-end decision (latest wins) |
| `SttInterim` | STT adapter | first interim transcript received |
| `SttFinal` | STT adapter | last final received before the LLM request (latest wins) |
| `TurnEnd` | turn state machine | end-of-turn decision taken |
| `LlmRequest` | LLM adapter | first request of the turn written to the connection. Each tool-loop round has its own record (`TimelineRecorder::llm_request`; up to `MAX_LLM_ROUNDS` = 32 per turn, see `docs/TOOLS.md`) |
| `LlmFirstToken` | LLM adapter | first text or tool-call delta received |
| `LlmFirstSentence` | TTS segmenter | first speakable chunk emitted |
| `TtsRequest` | TTS adapter | first text of the turn sent to TTS |
| `TtsFirstByte` | TTS adapter | first audio byte received |
| `FirstAudioOut` | output pacing/transport | first bot audio frame handed to the transport |
| `Interrupted` | turn state machine (`interrupt`) | barge-in accepted |
| `AudioOutStopped` | engine: the turn driver when its fast-path `Clear` is applied (else `TransportOut` at the `Interrupt`); reference: output pacing | bot playout flushed after the barge-in (first wins) |
| `VadStop` | engine VAD worker (`mark_open_turn`) | the VAD confirmed the stop: the decision, not backdated |
| `SttFinalize` | engine STT stage (`mark_open_turn`) | the `Finalize` for that stop handed to the provider session |
| `FirstChunk` | engine segmenter stage | the first chunk of the response leaves the segmenter; marked once per response (a comparison per later chunk, no lock), so with `TtsFirstText` it times the hop between the two stages |
| `TtsFirstText` | engine TTS stage | the first non-blank text of the response reached the TTS stage, once per response, with `LlmFirstSentence` and `TtsRequest`. `TimelineRecorder::is_marked` lets a stage skip a mark another one made: `TransportOut` no longer repeats the turn driver's `AudioOutStopped` |

Derived per-turn metrics (`TurnRecord::metrics`; logged as `turn finished`, target
`brisvox::timeline`, numeric microsecond fields, absent when `None`):
- `end_of_turn_delay` = `TurnEnd - UserSpeechEnd`
- `stt_final_delay` = `SttFinal - UserSpeechEnd`
- `llm_ttft` = `LlmFirstToken - LlmRequest`; `llm_first_sentence` = `LlmFirstSentence - LlmRequest`
- `tts_ttfb` = `TtsFirstByte - TtsRequest`
- `post_endpoint_latency` = `FirstAudioOut - TurnEnd` (providers + internal overhead after the
  end-of-turn decision)
- `response_latency` = `FirstAudioOut - UserSpeechEnd` (server-side V2V: excludes transport and
  client buffering, so V2V as defined above is always larger)
- `time_to_silence` = `AudioOutStopped - Interrupted` (barge-in responsiveness)

Overhead (`timeline::tests::overhead_per_event_is_small`, a turn rotation every 13 events, no
subscriber): ~14 ns/event release, ~60 ns debug (Windows x86-64). Per-event logs are TRACE
(enabling TRACE costs more than 1 us/event: formatting); summaries INFO, computed only when
enabled. History: last 32 finished turns (grown lazily); exporters use `drain_finished()`.

## Budget (rough; the engine's own share is measured: "Engine stage times and memory")
| Stage | Typical | Levers (task) |
|---|---|---|
| End-of-turn decision | 200–800 ms | adaptive policy, Smart Turn, STT signals |
| STT final | 100–300 ms | finalize on VAD stop |
| LLM first token | 200–500 ms | speculative start, warm connections, prompt caching |
| First speakable chunk | 20–150 ms | early-clause segmenter |
| TTS first byte | 100–250 ms | pre-opened WS contexts, cached phrases |
| Transport + buffers | 50–100 ms | 20 ms frames, minimal buffering, no extra resample |

## Principles
1. Overlap stages instead of waiting (speculation, streaming everywhere).
2. Nothing is set up on the critical path (connections, TLS, model sessions are warm).
3. Measure every stage of every turn (TurnTimeline) — no unmeasured claims.
4. Regressions are bugs: benchmark results are committed and compared.

## In-process load baseline
*Measured before the production pacer's per-session phase was turned off by default (2026-10-08 and 2026-10-13) and not re-measured: `sim-load` runs the reference pipeline of `brisvox-sim`, whose pacer never had that phase, so the change does not apply to these numbers (the sim-load and saturation tables below).*

`brisvox-bench sim-load` runs N simulated calls concurrently in one process, in real time, on a
multi-thread runtime: the full reference pipeline of `brisvox_sim::session::SimSession` per call,
3 scripted user turns each, mock providers with log-normal latencies (medians: STT final 150,
LLM first token 350, TTS first byte 150 ms). Result: `bench/results/sim-baseline.md`
(regenerate: `cargo run --release -p brisvox-bench -- sim-load --workers 4 --out
bench/results/sim-baseline.md`; the binary refuses to write it, and exits non-zero, if any call
failed, any turn went unanswered, a barge-in happened, tasks leaked or marks were missing).

Metrics:
- **Overhead** per turn = `(FirstAudioOut - TurnEnd) - (LlmFirstSentence - LlmRequest) -
  (TtsFirstByte - TtsRequest)`. It covers the hops `TurnEnd -> LlmRequest` and
  `TtsFirstByte -> FirstAudioOut` (also reported separately). It does **not** see a late wake-up
  inside a provider interval (counted as provider time, since marks are taken on receipt) nor
  anything before `TurnEnd` (input frames, VAD, STT, the end-of-turn wait).
- **Wake** = how long a task woken from outside the runtime (a plain thread, every 5 ms) waits
  until it runs: the scheduling pressure every hop sees, including the ones overhead cannot.
- **CPU cores** = process CPU time / wall time on the plateau (every call started, none
  finished), so ramp, linger and settle do not dilute it.
- RSS per call = (peak - before) / N; it includes fixed process growth, so the 500-call figure is
  closest to the marginal cost (the 100 -> 500 slope agrees: ~132 KiB/call).

Baseline 2026-10-08 (Windows x86_64, 32 logical cores, release, 4 tokio workers; a shared
desktop with other workloads running, so maxima are noisy): 500 calls -> 1500/1500 turns,
overhead p50 0.17 / p99 1.7 ms (max 6.0), wake p50 0.04 / p99 1.8 ms, 0.37 cores on the plateau,
~132 KiB peak RSS per call, ~13 tasks per call, 0 leaked. Same 500 calls on **1 worker**
(`bench/results/sim-500-1worker.md`): overhead p99 1.9 ms, wake p99 4.0 ms, 0.30 cores. An
earlier, quieter run of the same command gave overhead p99 ~1.1 ms: run-to-run spread on this
host is about 2x at the tail.

What this does and does not show:
- Worker threads are not a CPU limit: on a 32-core host the 4 (or 1) workers, mock timers, the
  timer driver and the probe threads may use any core. Headroom here comes from low utilisation
  (< 0.4 cores), not from proven 4-core capacity. A Linux run confined to 4 cores (taskset),
  pushed towards saturation, is in "Linux, confined to 4 cores" below.
- Windows timers tick at ~15.6 ms and coalesce wake-ups into bursts; Linux (1 ms) will look
  different. Production runs on Linux.
- Lower bounds only: mocks do no TLS, WebSocket, JSON or codec work, the energy VAD is one RMS
  pass per 20 ms frame, and nothing uses the inference pool. Real stages (model VAD / turn
  detection at 50 frames/s/call, provider I/O) are measured with the V2V and capacity benches below.
- Session phases: calls start 2 ms apart over a 1 s ramp, so 20 ms frame deadlines cluster
  (burstier than real traffic: conservative).

**Linux, confined to 4 cores (measured 2026-10-13; `bench/sim_load_linux.sh` reproduces it).** The same `sim-load` (3 turns per
call, mock providers in-process, real time) run natively in WSL2 Ubuntu (Linux 6.6.87.2-microsoft-
standard-WSL2) on the 32-core Windows 11 PC, under `taskset -c 0-3` with 4 tokio workers
(`available_parallelism` reads the affinity: 4). Files `bench/results/sim-baseline-linux.md`
(the Windows command plus `taskset`) and `sim-saturation-linux-run{1,2}.md` (two identical sweeps,
`--sessions 500,1000,2000,3000,3500,4000,5000,6000,8000,10000,12000`); `sim-saturation-linux-first.md`
is a first sweep kept for honesty (below).

| calls | answered | overhead p99 | wake p99 | CPU cores | peak RSS |
|---:|---:|---:|---:|---:|---:|
| 10 | 30/30 | 0.105 ms | 0.128 ms | 0.02 | 8.0 MiB |
| 100 | 300/300 | 0.075 ms | 0.117 ms | 0.06 | 20.2 MiB |
| 300 | 900/900 | 0.116 ms | 0.100 ms | 0.14 | 49.3 MiB |
| 500 | 1500/1500 | 0.185 ms | 0.133 ms | 0.27 | 75.1 MiB |

Against the Windows baseline at 500 calls (overhead p99 1.675 ms, wake p99 1.750 ms, 0.37 cores)
Linux is about 9 times better at the tail on 4 cores; the acceptance of "overhead p99 < 10 ms at
300 simulated sessions on 4 cores" is met by a factor of 85. The saturation sweep (overhead and
wake p99 in ms, runs 1 / 2, same 4 cores):

| calls | overhead p99 | wake p99 | wake max | CPU cores | peak RSS MiB |
|---:|---:|---:|---:|---:|---:|
| 1000 | 0.276 / 0.235 | 0.121 / 0.104 | 0.265 / 0.631 | 0.49 / 0.44 | 135 / 135 |
| 2000 | 0.442 / 0.348 | 0.148 / 0.204 | 0.598 / 5.98 | 0.81 / 0.66 | 263 / 263 |
| 4000 | 0.665 / 0.724 | 0.536 / 0.574 | 1.51 / 15.3 | 1.25 / 1.22 | 562 / 599 |
| 6000 | 1.040 / 0.910 | 1.326 / 1.002 | 9.37 / 4.59 | 1.73 / 1.64 | 804 / 823 |
| 8000 | 1.300 / 1.119 | 6.235 / 2.008 | 13.4 / 4.61 | 2.25 / 2.17 | 1230 / 1043 |
| **10000** | 1.497 / 1.492 | **17.848 / 19.551** | 26.7 / 36.4 | 2.76 / 2.75 | 1485 / 1298 |
| 12000 | 1.863 / 1.592 | 29.479 / 24.387 | 38.7 / 31.2 | 3.10 / 3.09 | 1740 / 1553 |

(5000, 3000 and 3500 are in the files.) Every step answered every turn (36000 of 36000 at 12000
calls), no call failed, no task leaked.

- **Where it saturates:** the first threshold crossing is **wake p99 > 10 ms at 10000 simultaneous
  calls** (17.8 and 19.6 ms; 8000 calls: 6.2 and 2.0 ms), at 2.75 of the 4 cores, with the **overhead
  p99 still 1.5 ms** (overhead p99 > 10 ms was not reached up to 12000 calls: 1.9 and 1.6 ms,
  with 3.1 cores busy). The scheduling delay of the tokio workers grows before the
  engine's own hops do, because the sim spends its time in timers and wake-ups.
- **A noisy first sweep.** `sim-saturation-linux-first.md` (run directly after the baseline)
  shows wake p99 13.4 ms and overhead p99 6.2 ms at 4000 calls and wake max 19.7 ms at 3000, which
  runs 1 and 2 did not reproduce (0.5 ms at 4000). The host is a shared Windows desktop under
  WSL2; the likely cause is host noise, not the engine, but it was not isolated. The conclusion
  therefore reads: wake p99 stays below 10 ms up to at least 8000 calls in the two repeats (the first
  sweep exceeded it at 4000 and 6000: 13.4 and 12.1 ms) and is above 10 ms from 10000 in both. Run-to-run spread at the
  tail is about 2x, as on Windows.
- Same caveats as above: mocks do no TLS, WebSocket, JSON or codec work and nothing uses the
  inference pool or a model, so this is the cost of orchestration and scheduling only. The
  capacity benchmark (above) is the number that includes the websocket, the pacer and the pool.

- Task leak check covers `tokio::spawn`ed tasks, not `spawn_blocking` or other pools.

Finding: the first run showed exactly 15.6 ms of overhead on every turn, all between TTS first
byte and the transport. The output pacer slept until an already-due deadline, and on Windows
such a sleep returns only at the next timer tick (~15.6 ms; tokio rounds deadlines up to its
1 ms wheel and the driver parks until the OS timer fires; the exact mechanism was not traced).
Rule: **never sleep on a due deadline on the hot path** — use `brisvox_core::clock::wait_until`,
which returns at once when `deadline <= now`. Applied to the pacer, the simulated input
transport, fixture playback and the mock providers' outboxes. Regression tests:
`brisvox-sim/tests/pacer.rs` (a counting clock: paused time cannot show the delay; plus a due
frame doomed by a queued interrupt is neither played nor logged as played).

## Resampling
`brisvox_audio::StreamResampler`: rubato `Fft` (exact rational ratios) for every pair except
downsampling to 8 kHz, which uses rubato's windowed sinc (see the sinc section below). One
instance per session and direction, PCM16 in/out, `flush()` drains the end of an utterance
(partial chunk + filter tail). Results: `bench/results/resampler.md` (`brisvox-bench resample`,
one warm instance) and `crates/brisvox-audio/tests/resample.rs`, `tests/sinc_design.rs`,
`tests/resample_memory.rs`.

**The trade-off is passband vs delay.** rubato's FFT filter is flat only if its block is long
enough at the LOWER of the two rates. Measured gain at 8 kHz <-> 16/24 kHz (telephony needs
flat to 3.4 kHz):

| FFT block | delay | 2.4 kHz | 3.0 kHz | 3.4 kHz | 3.6 kHz |
|---|---:|---:|---:|---:|---:|
| 5 ms (40 samples) | 2.5 ms | -2.1 dB | -18 dB | -45 dB | -66 dB |
| 10 ms (80) | 5 ms | 0.0 | -0.1 dB | -10 dB | -29 dB |
| 20 ms (160) | 10 ms | 0.0 | 0.0 | 0.0 | -2.0 dB |

Chosen: a block of **160 samples at the lower rate** (rounded up to the rates' exact rational
step): flat (< 0.2 dB) to 0.85 x the lower Nyquist, enforced by the sweep test.
- **Filter delay** = half a block: **10 ms when the lower rate is 8 kHz, 5 ms at 16 kHz, 3.3 ms
  at 24 kHz** (6.7 ms for 22.05 kHz <-> 24/48 kHz, 10 ms for 22.05 kHz <-> 16 kHz). A test checks
  the reported delay against the actual position of a click in the output.
- **CPU 0.7-3.2 µs per 20 ms frame** (median 1.9; one warm instance). Inline on the async path.
  No allocation after construction (counting-allocator test).
- **Cache cost** with many instances (one per session, cycled round-robin, release, 2026-10-08,
  wall-clock, one machine): FFT 48 kHz -> 16 kHz 1.4-2.4x per frame over three runs (the bench
  note of ~1.5-2x); sinc 24 kHz -> 8 kHz 1.0-1.1x.
- SNR 89-100 dB at 1 kHz and at 0.85 x Nyquist; a full-scale tone above the output Nyquist
  leaves nothing above the 16-bit floor.
- Buffering: output per whole chunk (= one block, `buffering()`): nothing extra for input
  aligned to it, up to one block otherwise.
- **Memory and construction** (release for timings, 2026-10-08): 22-31 KB retained per instance
  and 10-20 µs to construct (FFT pairs). Sinc pairs are far larger, see below.
History: the first version used 2.5 ms blocks (and quoted "2.5 ms delay"); the review's gain
sweep showed it cut telephony speech above ~2.4 kHz, so delay was traded back for a correct
passband.

**Downsampling to 8 kHz uses a windowed-sinc filter instead.** The FFT cannot do
better than 10 ms at 8 kHz output with a flat passband (above). A windowed sinc (rubato
`Async::new_sinc`, BlackmanHarris2 window, cubic interpolation, 128 oversampling phases) can,
at a length per source rate. Length sweep: `tests/sinc_design.rs::probe_sinc_length_sweep`
(`cargo test --release -p brisvox-audio --test sinc_design -- --ignored --nocapture`). Quality
bar: |gain| <= 0.2 dB at 3.4 kHz, >= -2 dB at 3.6 kHz, >= 55 dB rejection of a 4.4 kHz tone
(which aliases to 3.6 kHz). The sinc is checked against that bar, and 6 kHz (aliasing to 2 kHz)
too, by `sinc_pairs_meet_the_passband_and_stopband_bar`.

| source | sinc length | delay (FFT: 10 ms) | gain 3.4 kHz | gain 3.6 kHz | 4.4 kHz alias | CPU per 20 ms |
|---:|---:|---:|---:|---:|---:|---:|
| 16 kHz | 192 | 6.0 ms | -0.05 dB | -1.70 dB | -140 dB | 5.5 µs |
| 22.05 kHz | 224 | 5.0 ms | -0.07 dB | -1.39 dB | -127 dB | 7.2 µs |
| 24 kHz | 224 | 4.6 ms | -0.12 dB | -1.61 dB | -102 dB | 6.2 µs |
| 44.1 kHz | 384 | 4.25 ms | -0.03 dB | -0.63 dB | -64 dB | 10.2 µs |
| 48 kHz | 512 | 5.25 ms | -0.00 dB | -0.17 dB | -82 dB | 12.4 µs |

(Sweep numbers are f32 rubato output for 1 s tones; the FFT row for comparison: gain 3.6 kHz
-2.03 dB, 4.4 kHz alias -157 to -171 dB at 16/24/48 kHz.) The lengths are the shortest that meet
the bar, except 48 kHz: 384 meets it at -55.6 dB (no margin), so 512 is used. Shorter lengths
fail: at 16 kHz, 160 gives -3.6 dB at 3.6 kHz; at 22.05 and 24 kHz, 192 gives -2.5 and -2.7 dB;
at 44.1 kHz, 320 gives -51.5 dB of 4.4 kHz alias. Note the rejection is far below the FFT's
(-157 dB) for 44.1 and 48 kHz: the sinc meets the 55 dB bar, it is not equal to the FFT.

Sinc costs (release, 2026-10-08, the sinc pairs only; `probe_construction_and_cache_cost`):

- **Memory**: the filter table is 128 x sinc_len x 4 bytes and is per instance (rubato does not
  share it). Retained per instance (`tests/resample_memory.rs`): 105 KB (16 kHz), 122 KB
  (22.05/24 kHz), 207 KB (44.1 kHz), 274 KB (48 kHz) (KB = 1000 bytes). 500 sessions at 24 kHz
  -> 8 kHz hold ~60 MB. The FFT pairs keep 22-31 KB.
- **Construction** (median of 30, two runs): 0.6 ms (16 kHz), 0.7 ms (24 kHz), 1.5-1.6 ms
  (48 kHz); the FFT pairs take 10-20 µs. Construction must run off the async workers
  (`StreamResampler::new` docs): 1.5 ms is a stall for a tokio worker (see CONTRIBUTING.md).
- **Cycling 500 instances**: 1.0-1.1x per frame (24 kHz -> 8 kHz, three runs), so the table's
  cache footprint has no measurable cost here.

Upsampling from 8 kHz gains only 2 ms with sinc (sinc length 128: delay 8.0 ms, -0.26 dB at
3.4 kHz, 10.6 µs per 20 ms vs the FFT's 0.9 µs, `probe_upsampling_sinc_versus_fft`). That is
below the 3 ms bar for ~12x the CPU and a filter that misses the 0.2 dB passband bar, so it stays
on the FFT. A telephony call (8 kHz in -> 16 kHz STT: 10 ms; 24 kHz TTS -> 8 kHz out: 4.6 ms) now
spends ~15 ms in resampling instead of 20.

## Output pacing
`brisvox_audio::OutputPacer` (production) releases audio downstream at real time, one frame
duration after the previous one, on the injected `Clock`. The reference pacer in `brisvox-sim`
is the behavioural spec. `crates/brisvox-audio/tests/pacer.rs` (15 tests, paused time, a
`CountingClock` that counts timer calls and can make timers fire late) pins the production
rules, including counterparts of the reference's regression tests.
- **Absolute schedule**: deadline = previous deadline + previous frame's own duration, so sleep
  overshoot does not accumulate. Tested three ways: 10 000 x 20 ms frames through a one-slot
  consumer that stalls 40 ms stay exactly on their grid after the stall (the one-slot inbox
  absorbs that stall, so the pacer does not lag here); a pacer that is itself late (its input
  arrives 50 ms late) releases the three overdue frames at once and is back on the grid at the
  next grid point; a 100 ms sink stall (lag up to 140 ms, within `max_lag`) releases the late
  frames together and re-acquires the grid from the next frame on. Rescheduling from the actual
  release instant instead fails the last two tests.
- **Lag**: a frame up to `max_lag` (default 60 ms) late is released at once and keeps the grid.
  Beyond `max_lag` the pacer resyncs to the clock, counts one underrun and restarts the grid
  (300 ms stall: exactly one underrun, then consecutive 20 ms frames). `PacerMetrics` reports
  frames played, underruns and the worst lag. A frame is counted only once it is delivered.
  **Worst lag** is measured at hand-off, against the frame's grid point, after the push returns:
  it includes the timer's overshoot (a timer that fires 15 ms late on Windows makes that frame
  15 ms late, also when the frame arrived early) and any time the push was blocked downstream. It
  can therefore exceed `max_lag`; only the underrun count uses the lag at arrival. Tests: a
  timer that fires 15 ms late with frames queued early (worst lag exactly 15 ms, no underrun);
  the pacer waking 3 ms late after the clock is advanced past its deadline in one step (worst
  lag 3 ms, the next frame on the absolute grid). Both fail if the lag is taken at arrival or
  the schedule is rebuilt from the wake instant.
- **Due deadlines never touch the timer**: every test asserts zero `sleep_until` calls on an
  already-due deadline, including the turn-end hold (a `TtsEnded` that arrives after the last
  frame's deadline goes out at once; mutant that sleeps through the timer fails). Tokio rounds every sleep up to its timer tick, ~15.6 ms on Windows
. A due frame doomed by an interrupt queued just before it is processed is neither
  released nor counted (test with a clock hook that enqueues the interrupt at the frame's first
  `now()`).
- **One timer per session**: the 10 000-frame test counts all `sleep_until` calls: 2 in total,
  the pacer's single pinned `Sleep` (re-armed with `reset`) and the sink's one stall.
- **Interrupt flush**: a wait is abandoned as soon as an interrupt for a newer epoch is queued
  (`EpochWatch`), the doomed frame is neither released nor counted, and the schedule resets so
  the next turn starts at once. A push already blocked on a full downstream inbox is abandoned
  the same way: the framework drops the frame and counts it in `stale_dropped`, and the pacer
  then neither counts it nor advances the schedule (test: interrupt at phase + 85 ms while frame
  4 is blocked; the sink log is exactly 0..=3 then the new turn, and 5 frames are counted).
  Mid-interval interrupt: no old frame after it. Interrupt while the sink holds a dispatched frame:
  exactly that frame plays after it (log order asserted exactly), and the new turn follows it.
- **Turn ends**: a `TtsEnded` is held until the turn's last frame has played out, then forwarded
  downstream, and the schedule restarts. A natural silence of 5 s before the next turn is not an
  underrun (test). The reference also reports `TtsEnded` upstream; the production pacer does not
  yet, because no production stage consumes that upstream report, and the stage that owns the
  turn will add it when it exists.
- **First frame at once** (the default): the pacing clock starts at the first audio frame
  after an idle period (session start, an interrupt, a finished turn). That frame leaves at its
  arrival, with no timer, and the 20 ms grid runs from there (tests: 256 seeds; a first answer
  after 2 s of idle; the first frame after an interrupt).
- **Phase** (opt-in, `OutputPacer::with_first_frame_phase(true)`; the former default): the
  session's first frame is released `phase_offset(seed)` after its arrival, a whole millisecond in
  `[0, 20 ms)` from splitmix64 on the seed. Same seed, same offset. Two sessions run concurrently
  with seeds 7 and 8 wake at 7 ms and 2 ms. The phase applies only to the session's first frame.
  The other tests of this file run with it on, because they pin the grid through it.
- **Frame durations**: pacing uses each frame's own duration (10, 20 and 40 ms tested).
- **Known equivalence**: the no-sleep fast path in `wait` also checks the interrupt queue, which
  mirrors the framework's own check in `push`. Removing the fast-path check changes no observable
  output, since `push` drops the same frame; the tests pin the observable outcome.
- **Not here**: transport-side buffers are cleared by the transport on interrupt; only the
  frame already handed downstream can still play.

## Inference pool
`brisvox_inference::InferencePool`: model calls (VAD, turn detection, heavy DSP) run on its own
OS threads, never on tokio workers. Bounded queue that **rejects** when full (`PoolSaturated`:
shed load rather than build latency), per-request timeout on the Clock (an unstarted job of a
timed-out request is skipped), panic isolation per job, lock-free metrics plus per-job
wait/run timing.

Load test (`bench/results/inference-pool.md`, `brisvox-bench infer-load --seconds 4 --costs-us
100,200,250,280`): 500 sessions x one job per 32 ms window (one Silero VAD window) on a 4-thread
pool; jobs busy-spin their cost. Two arrival modes: `paced` (one thread spin-paces jobs evenly,
open loop: measures the pool) and `timers` (each session a tokio task on a timer, closed loop:
what an engine stage would do, but on Windows its timers fire in ~15.6 ms batches, so jobs
arrive in bursts of ~200). Windows, shared desktop:

| job cost | offered load | paced wait p99 | timers wait p99 | outcome |
|---:|---:|---:|---:|---|
| 100 µs | 0.39 | 0.11 ms | 4.2 ms | all served |
| 200 µs | 0.78 | 0.62 ms | 11.4 ms | all served |
| 250 µs | 0.98 | 1.14 ms | 14.4 ms | all served |
| 280 µs | 1.09 | 73 ms | 34 ms | paced: queue full (1024), 8% rejected; timers: 9% windows missed |

Reading it:
- The pool itself adds ~10-20 µs per job (cross-thread wake + oneshot) and queues fairly up to
  the edge: wait stays around a millisecond up to 98% load. Beyond 100% it falls off a cliff —
  the queue fills, waits jump to tens of ms, and jobs are rejected (open loop) or windows missed
  (closed loop). Never unbounded memory.
- The `timers` rows measure the load generator's burstiness (wait p99 ~ burst x cost / threads),
  not the pool: real VAD jobs are triggered by arriving audio, which is smoother. Linux timers
  (1 ms) are much less bursty.
- Plan for <= 70% pool load. Assuming ~1 ms per window for Silero VAD on one CPU thread
  (UNVERIFIED when this plan was written), per-window VAD costs ~3% of a core per session, so a 4-thread
  pool carries ~90 sessions with headroom (and the tokio workers share those cores). Hundreds of
  calls per container need cross-session batching, an energy pre-gate before the model, or more
  cores — an input for the VAD and Smart Turn work (docs/RISKS.md).
- Queue capacity is not a latency bound (1024 jobs of 1 ms are 0.25 s of wait on 4 threads); the
  per-request timeout is.

## TTS segmentation
Code: `brisvox_agent::segment` (`Segmenter`, `SegmenterConfig`, `timed_chunks`). The segmenter
turns LLM text deltas into chunks for TTS. It only decides where chunks end and reports nothing to
the timeline: the caller records `LlmFirstSentence` when it receives the first chunk, which is the
moment the first audio can start. `timed_chunks` is a test helper that feeds timestamped tokens and
reports the token that released each chunk.

What is emitted when (`first_min_words` 3, the `SegmenterConfig` default; the engine's
production default is one word, see "First TTS chunk" below; `max_chunk_chars` 250):
- **First chunk** of a stream: at the earliest clause boundary (`,` `;` `:` followed by
  whitespace, or a spaced dash ` - `) or sentence end whose chunk already holds 3 words.
- **Later chunks**: only at sentence ends (`.` `!` `?` `…`, runs such as `?!`, closing quotes,
  brackets or markdown emphasis after them, as in `**Done.**`), and at a newline after text.
  Deliberate deviation from "sentence ends only": LLM lists and paragraphs need a pause at a line
  break, and without it `1. Buy milk\n2. Call mom` becomes one run-on utterance. A line that ends
  in `,` `;` `:` or a dash ends its chunk at the newline too (`- eggs,\n- milk,` is two items,
  `Here is the list:\n1. Buy milk` pauses after `list:`); a line that ends in a terminator is left
  to the terminator rules, so `1.` stays with its item. A bare `-` bullet line is not a chunk of
  its own.
- **Over the limit** (250 chars by default): pending text without a sentence end is cut at the
  last clause boundary inside the limit, else at the last whitespace inside it, else at the first
  whitespace after it, so the over-long token stays whole and the stream keeps flowing.
- **`finish()`** (stream end): emits what is left and resets the segmenter for the next stream.
- **`reset()`** (barge-in): drops pending text, which was never spoken, and the next chunk is a
  first chunk again.

**One-token lookahead.** A period, `?`, `!`, `…` or clause mark is decided only when the next
non-space character has arrived, because `Dr.` + ` Smith`, `3.` + `14` and `3.` + ` sinif` differ
only in what follows. The cost is at most one token after the boundary: the next token carries
that character. A newline is unambiguous and is decided at once.

**Never split inside a token.** Decimals (`3.14`, `3,14`), thousands (`1,000`, `1.000`), times
(`10:30`), Turkish ordinals and list markers (`3. sinif`, `1. Buy`), abbreviations (per language,
case-insensitive, dotted and dotless i folded together), URLs and e-mail addresses (a period that
ends one is a boundary only before a capital letter or a digit), dotted acronyms (`E.U.`, `B.C.`,
`N.Y.C.`: not a boundary), ellipses before a lowercase word. A number and period mid-line followed
by a capital (`It is done now. 2. We left.`) is a sentence end; at a line start it is a list
marker and is not. Cuts are made only after punctuation or at whitespace, so emoji ZWJ sequences,
flags and skin tones stay whole.

**Over-limit fallbacks never cut after a declined period.** The whitespace fallback (the last
whitespace inside the limit, else the first after it) skips a whitespace that follows a period the
rules declined (`Dr.`, `vb.`, `E.U.`, `3.` before `sinif`, `…` before a lowercase word): such a
token is not a boundary, even when the limit forces a cut. `Oğlum 3. sinif öğrencisi` with a limit
of 10 gives `Oğlum`, `3. sinif`, `öğrencisi`, not `Oğlum 3.`. Whether a period is declined can
depend on the next non-space char: when that char has not arrived, the fallback waits for it,
unless every possible next char gives the same decision (so a word such as `Yes.` never waits).

**Long tokens and whitespace.** A token without whitespace stays whole up to 4 × `max_chunk_chars`
(1000 chars by default). Past that, the cut is made at the nearest grapheme-safe char boundary: the
one exception to "cut only at whitespace or after punctuation". Grapheme-safe means no cut inside a
ZWJ sequence, a variation selector, a flag, a skin tone or a combining mark of the common scripts
(Latin and Cyrillic diacritics, Hebrew points, Arabic harakat, Devanagari and Thai signs, kana
voiced marks; a subset of Unicode `Mn`/`Mc`, not all of them). A single grapheme longer than the
cap (for example `x` plus 1200 combining acutes) has no safe boundary, so it is cut at the cap
itself: memory stays bounded, and the rest is not emitted one mark per chunk. Whitespace runs
longer than `max_chunk_chars + 1` are shortened to that length (a newline inside the excess is
kept). So the pending text stays at or below 4 × `max_chunk_chars` + 2 chars after every push,
plus at most `max_chunk_chars + 1` whitespace chars while a fallback waits for the next char after
an ambiguous period (5 × `max_chunk_chars` + 1 in all), and a long token never stalls the output
for the rest of the answer. Over-limit cuts wait for the lookahead of every
boundary inside the limit, so the output is the same however the deltas are split (tested with
1-3 char pieces, random splits and random token streams).

Measured on token streams (`tests/segment.rs`, one token every 30 ms, `timed_chunks`): for eight
realistic answers (4 EN, 4 TR) the first chunk leaves on the token after the first eligible
boundary, i.e. 30 ms after the boundary token. These are segmenter numbers, not TTS numbers: real
LLM inter-token jitter adds to them.

| Lang | First chunk | Boundary token | Emitted at |
|---|---|---|---|
| EN | `Sure, I can help you with that.` | `.` (token 8) | token 9 (+30 ms) |
| EN | `The forecast is rain,` | `,` (token 4) | token 5 (+30 ms) |
| EN | `Hi! How can I help today?` | `?` (token 7) | token 8 (+30 ms) |
| TR | `Merhaba, bugün size nasıl yardımcı olabilirim?` | `?` (token 7) | token 8 (+30 ms) |
| TR | `Tabii ki, hemen bakıyorum.` | `.` (token 5) | token 6 (+30 ms) |
| TR | `Toplam 1.000 kişi geldi.` | `.` (token 6) | token 7 (+30 ms) |
| TR | `Oğlum 3. sinif öğrencisi.` | `.` (token 5) | token 6 (+30 ms) |
| EN | `**Warning: this is bad.**` | `.**` (token 6) | token 7 (+30 ms) |

## Segmenter push cost in the stage
`SegmenterStage` runs `Segmenter::push_spans` on a tokio worker for every LLM delta, so it falls
under the 50 us rule of CONTRIBUTING.md. The cost is one scan of the pending text (at most
`5 x max_chunk_chars` chars) plus one allocation per emitted chunk, plus, for text with sentence
ends, up to about two allocations per push while a candidate period waits for its lookahead
character (the abbreviation check re-reads the token before it; measured with a counting
allocator, 1.6 per push at 4-char deltas and 2.3 at 12-char deltas on 120 sentences, none for text
without sentence ends beyond the chunks); the stage keeps the raw stream
text it has not cut yet and ends the stream early when that passes `RAW_CAP_FACTOR` x
`max_chunk_chars` bytes (a whitespace flood, see the module docs of `segmenter.rs`).

Measured in release (2026-10-08, Windows dev machine, deltas of 4 to 12 chars, best of 3 runs for
the worst push), `a_push_costs_less_than_the_worker_budget` in
`crates/brisvox-engine/tests/segmenter_stage.rs` (prints the numbers with `--nocapture`):

| Input | Median | p99 | Worst |
|---|---|---|---|
| 120 sentences (EN / TR config) | 0.2 us | 0.7-1.1 us | 1-4 us |
| 150 repetitions of a long clause without punctuation | 0.3-0.4 us | 0.9-1.1 us | 2-10 us |
| 12,000 chars of space-newline pairs between two letters | 0.6 us | 0.6-1.3 us | 1-7 us |
| 12,000 chars without any whitespace (one token) | 1.1-1.2 us | 2.6-2.8 us | 15-109 us |

All far inside the 50 us rule; the worst push is the hard cut of an over-long token, which scans
the whole pending text once, on a path no real LLM output takes. The test asserts, per input and
language: median under 50 us (release) / 500 us (debug), p99 under 500 us / 5 ms, worst under
2 ms / 50 ms, each statistic the best of three runs (the minimum, so one scheduling hiccup on a
loaded runner does not fail it). The tail bounds are loose multiples, there to catch a quadratic
`drain`, not to prove the 50 us rule for the worst push; the median is the rule.

The gate (`scripts/verify.sh`) runs the tests in debug, where those bounds are an order of
magnitude above the release medians, so a 50x slowdown would still pass them. The
machine-independent guard is `crates/brisvox-engine/tests/segmenter_alloc.rs`: a counting
allocator over the same four input shapes, asserting at most 4 allocations per emitted chunk (plus
3 per push for text with sentence ends, plus a constant), which a per-character allocation
(verified by mutation) exceeds. The release wall-clock bounds
are enforced by `cargo test --release -p brisvox-engine --test segmenter_stage`.

## TTS text normalisation
Code: `brisvox_agent::normalize`; rules, tables and limits: `docs/TURKISH.md`. The normaliser turns
written forms (`%15`, `1.250.000`, `150 TL`, `12.03.2026`, `14:30`, `Dr.`) into the words to
speak, per segmenter chunk, in Turkish and English. **Status: wired.** `TtsStage::spoken`
normalises every chunk on its way to the TTS provider, after the segmenter, so it adds no wait: a
chunk leaves the segmenter exactly when it did before. A chunk of up to `INLINE_CHUNK_CHARS` (256)
is normalised inline on the worker (the 50 us rule holds, see below); a longer one (the segmenter
hard-cuts at up to 5 x `max_chunk_chars` = 1,250 chars) goes to the `Offload`, and an offload that
fails or times out speaks the written text instead (counted as `normalize_failures`). The raw
chunk, not the spoken form, is what the playout book records as sent text. The cost below is
therefore part of the time from the segmenter's chunk to the provider's first text, not of the
segmenter's own wait. (Tests: `chunks_are_spoken_normalized_but_recorded_verbatim`,
`a_long_chunk_is_normalized_on_the_offload_and_gives_the_same_text` in
`crates/brisvox-engine/tests/tts_stage.rs`.)
Cost: linear in the chunk; a chunk with no digit, symbol or period is returned borrowed, one that
comes out unchanged too. Measured in release (2026-10-08, dev machine, 20,000 calls each; re-run
of the digit-dense shapes after the review fixes): 0.7 us for a 14-char chunk, 4 us for 250 chars
without numbers, 9-10 us for a 328-char mixed Turkish/English chunk (numbers, date, time, phone),
40 us for a 1,230-char mixed chunk, and about 0.1 ms for 1,250 chars of pathological digit-dense
text (`1 2 3 ...`). For a 256-char (`INLINE_CHUNK_CHARS`) digit-dense chunk the cost is 11-26 us in
Turkish and 15-45 us in English; the worst shape (`1 2 3 ` repeated, English) is 45 us, so the
50 us rule for work on a tokio worker holds for chunks up to `INLINE_CHUNK_CHARS` (256; the
segmenter's default `max_chunk_chars` is 250) but with little margin on that shape, not with
room to spare. Chunks above `INLINE_CHUNK_CHARS` are normalised off the worker, as above.

What the tests guard (`crates/brisvox-agent/tests/normalize.rs`; the absolute bounds depend on
the build profile, so `cargo test --release -p brisvox-agent --test normalize` is the run that
checks the numbers above):

| Test | Bound | Catches |
|---|---|---|
| `a_typical_chunk_costs_microseconds_not_milliseconds` | 328-char mixed chunk, mean under 2 ms in release (200 times the measurement), 20 ms in debug | a constant-factor blow-up (a `Vec<char>` rebuilt per token) |
| `a_digit_dense_inline_chunk_stays_within_the_worker_budget` | 256-char digit-dense chunks of five shapes, mean under 500 us in release (10 times the 50 us rule), 10 ms in debug | a slow path on dense numbers; it does not prove the 50 us rule, the table above is the (unguarded) measurement |
| `cost_grows_linearly_with_the_input` | 16 times the input must cost under 64 times the time, best of 5, 18 shapes (digit runs, dotted groups, parentheses, space groups, currency, abbreviations, long letter runs, `Dr.Dr.Dr.`, period runs) | an accidental rescan (quadratic) |

The 50 us claim itself and the 10 us / 40 us figures are measurements, not assertions; the
bounds are deliberately loose so a noisy machine does not fail the suite.

Turkish and mixed-language call latency is measured with real speech by `brisvox-bench fixtures`
(`bench/results/fixtures-sim.md`, section "3. Turkish fixtures in the benchmarks" in
`docs/TURKISH.md`).

## End-of-turn model
Smart Turn v3.2 runs once per candidate end of turn (a VAD stop), on the inference pool:
features ~1.0-1.2 ms + model ~22.5-28 ms on one thread (Ryzen 9 9950X, Windows, release;
median of 15; bench/results/smart-turn.md), so ~25-30 ms is added between the VAD stop and the
verdict. Pipecat runs the same model on the same runtime (24.2 ms model, 2.1 ms numpy features
on this machine), so brisvox saves ~1 ms of features and otherwise matches it. On the caller's
thread only the 8 s ring copy runs (~2 us). Levers, measured in the Smart Turn latency work: more intra-op threads for this one job
(latency vs pool capacity), and starting the analysis speculatively when VAD probability first
falls (before `min_silence` confirms the stop) so the verdict is ready at the stop.

### Smart Turn intra-op threads (measured 2026-10-08)
onnxruntime 1.28.0, Ryzen 9 9950X, median of 31 warm runs on the golden features:

| intra-op threads | median | p90 | CPU per call (upper bound) |
|---:|---:|---:|---:|
| 1 | 25.9 ms | 27.1 ms | ~26 ms |
| 2 | 16.8 ms | 18.8 ms | ~34 ms |
| 4 | 12.6 ms | 13.6 ms | ~50 ms |
| 8 | 9.9 ms | 10.6 ms | ~79 ms |

Default is 2 threads (`DEFAULT_INTRA_THREADS`): -9 ms on every end-of-turn verdict for ~30% more
CPU per analysis (~0.8% of a core per talking session instead of ~0.6%, docs/RISKS.md). Probabilities
stay within the 1e-6 golden tolerance at 1, 2 and 4 threads, through warmed sessions
(`probabilities_match_the_reference_at_1_2_and_4_threads` in `crates/brisvox-inference/tests/smart_turn.rs`);
the default (2) and the clamp of 0 to 1 are pinned by `the_thread_setting_defaults_to_2_and_is_at_least_1`.
Every session (first, warmed, cold start) is built by one method from the stored count. That the
setting reaches ONNX Runtime is a timing fact, so it is checked by the ignored release test
`more_threads_cut_the_latency_of_warm_and_cold_sessions` (`ort_backend.rs`; 2 threads must be
< 0.85x of 1 thread, for a warmed and a cold-start session). Re-run 2026-10-12 (all-zero
features, other builds running): 1 / 2 / 4 threads = 22.9 / 12.5 / 8.1 ms warmed, 23.0 / 12.8 /
7.9 ms cold start; with every session forced to 1 thread the test fails (26.3 / 25.5 / 26.9 ms).
So the pyke-provided runtime honours `with_intra_threads` (not an OpenMP build that ignores it).

### Early analysis: Smart Turn at the first VAD fall (measured 2026-10-08)
Silero's probability falls below the negative threshold within one or two windows (32 ms each) of the last speech,
but the stop is only confirmed after `min_silence` (100 ms by default). With
`EngineConfig::early_analysis` enabled the VAD worker sends `SystemFrame::UserSpeechFalling` once
the fall has lasted `fall_delay` (default 64 ms), the `TurnDriver` snapshots the ring and runs
Smart Turn at once, and at the confirmed stop it answers the machine's request with that verdict
when the reuse rule holds (same fall: the stop carries the hint's time; at most `max_tail` =
300 ms of audio since the snapshot; no newer hint or speech start; module docs of
`turn_driver`). Otherwise the normal analysis runs. Off by default: no hint frame exists then.

Measured with the real Silero (tract) and Smart Turn (ONNX Runtime 1.28, 2 intra-op threads),
release build, Ryzen 9 9950X, the three fixtures (6 utterance ends), by
`crates/brisvox-engine/tests/early_analysis_realtime.rs` (features `silero-tract smart-turn-ort`):

Agreement, verdict at the hint vs verdict at the stop of the same fall (audio time):

| min_silence | hint delay | falls | stops | paired | same verdict | max abs dp | hint-to-stop | wasted |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 100 ms | 0 / 32 ms | 7 | 6 | 6 | 6 | 0.007 | 96 ms | 1 |
| 100 ms | 64 ms | 6 | 6 | 6 | 6 | 0.050 | 60 ms | 0 |
| 200 ms | 64 ms | 6 | 6 | 6 | 6 | 0.022 | 160 ms | 0 |
| 300 ms | 64 ms | 6 | 6 | 6 | 6 | 0.065 | 260 ms | 0 |
| 500 ms | 64 ms | 6 | 6 | 6 | 6 | 0.056 | 443 ms | 0 |

(The fixtures are clean read sentences: 6 stops and one inner dip that a 64 ms delay filters out.)
Stability of the verdict under the extra silence between hint and stop on far more points:
- real utterance ends with the real room noise that follows (hint after 48-128 ms of it, stop
  32-128 ms later): 144 of 144 pairs give the same verdict, max abs dp 0.105;
- stress test, every utterance cut every 100 ms from a third of its length (an abrupt cut
  stands in for a pause inside a sentence) with digital silence appended (96 ms vs 160 ms):
  190 of 211 (90.0%) agree, max abs dp 0.83. The 21 flips are 19 times "complete only at the
  stop" (the early verdict is the more cautious one: the turn would wait for the fallback or the
  STT punctuation instead of ending at once) and 2 times "complete only early". This int8 model
  is jumpy (docs/providers/smart-turn.md), so a borderline verdict can move with 64 ms of extra
  silence; the early verdict is one the model gives for the same audio minus that silence.

Latency in the production stages (`AudioIn` + Silero worker on the pool -> `TurnDriver` + Smart
Turn on the same 3-thread pool, paced in real time, six runs in parallel, so the base figure
includes some contention; 2 stops per fixture):

| mode | mean stop-to-verdict | analyses started | early hints / started / reused / ready / wasted |
|---|---:|---:|---|
| base | 23.8 ms (22.9-25.1) | 6 | - |
| early | 0.0 ms | 6 (all early) | 6 / 6 / 6 / 6 / 0 |

The test asserts, in early mode, that every fixture reuses at least one early verdict, that no
early job fails, and that at least 5 of the 6 stops reuse theirs (one stop of slack for a loaded
machine), so a hint whose dating did not survive `AudioIn` -> `TurnDriver` (every early job
wasted, the normal analysis at every stop) fails it. A debug run on a loaded machine (2026-10-12)
still reused 6 / 6 (adopted while running, 0 ready at the stop, 0 wasted).

"Stop-to-verdict" is the time from the VAD stop reaching the driver to the verdict being fed to
the turn machine. All six verdicts were ready when the stop arrived: -23.8 ms off the
VAD-stop-to-verdict path at no extra analysis (the early analysis REPLACES the one at the stop).
Where it counts: the end of a turn waits for the slower of the verdict and the STT final
(finalize is sent at the stop and runs concurrently), so the saving shows up in the turn end
only when the final arrives within ~25 ms of the stop (a fast STT, a text-free policy such as
`require_final = false` on a provider that answers a finalize quickly), and in full when the
verdict was the slower one.

Cost: every hint is one Smart Turn job; a fall that does not become a stop wastes one (counted as
`early_wasted`, same CPU as the analysis at the stop, ~34 ms with 2 threads). On the fixtures: 0
wasted at the default 64 ms delay, 1 per 6 stops at 0-32 ms. Natural speech pauses more than the
read sentences, so `max_per_turn` (default 4) caps the early analyses per user turn. Budget
(docs/RISKS.md: one analysis per stop is ~0.7% of a core per talking session with 2 threads, ~2
cores for 300 sessions): the measured waste of 0-17% costs ~0-0.35 cores more at 300 sessions.
The cap is the guarantee, not the forecast: a speaker who hit it in every turn would need up to
4 extra jobs per turn, several times the base rate (6-10 cores at 300 sessions), so a deployment
should watch `early_wasted / vad_stops` (and `early_capped`) and raise `fall_delay` or turn the
feature off if it climbs; the base rate and the 2-core figure are unchanged when it is off.

Configuration constraints (checked by `VadParams::falling_hint_possible`, warned at
`VadSetup::with_early_analysis`; `EngineConfig::validate` cannot see the VAD parameters):
- `fall_delay`, rounded up to whole 32 ms windows, must be below the VAD `min_silence` (up to 96 ms at
  the default 100 ms); otherwise the stop is confirmed by the window that would hint and no hint is
  ever sent (`VadStatsSnapshot::falls == 0`, feature inert);
- `speech_pad <= min_silence`: the stop is then always dated `last speech end + speech_pad`, so the
  hint carries exactly its time even when the pad is longer than the silence seen at the hint (the
  hint may be dated slightly ahead of its own window; the driver then waits for the ring to reach it,
  as the normal path does). With a longer pad the stop's time depends on the confirming window, so
  no hint is sent. Tested with pads of 0-100 ms against minimum silences of 100-300 ms.
- Both halves must be enabled: `EngineConfig::early_analysis` (driver) and
  `VadSetup::with_early_analysis` (VAD). Only the first one set: no hints, `early_hints == 0`.

"Silence only since the snapshot" is decided by the VAD, not re-measured: windows between the
negative and the positive threshold after the hint neither cancel the pending stop nor restart its
silence, so a stop can be confirmed with the hint's time after band-level audio; that verdict is
reused too (pinned by `a_band_level_window_between_the_hint_and_the_stop_still_reuses`).
Counters reconcile: `early_started == early_reused + early_wasted + early_failed` when none is in
flight (turn end, `Cancel` and hangup count a started analysis as wasted, and a late verdict of
a discarded job changes nothing; pinned in `crates/brisvox-engine/tests/early_analysis.rs` by
`the_end_of_the_turn_discards_a_still_running_early_job`, `cancel_counts_a_started_early_analysis_as_wasted`,
`hangup_counts_a_finished_early_analysis_as_wasted` and
`hangup_discards_a_running_early_job_and_its_late_verdict_changes_nothing`). The `max_per_turn`
budget is renewed at each new user turn (`the_per_turn_budget_is_renewed_for_the_next_turn`), and
`fall_delay` reaches the VAD from `EngineConfig` (`vad_fall_hint.rs`: 40 ms hints on a 60 ms dip,
80 ms does not; a delay >= `min_silence` gives `falls == 0`).

Decision: ADOPTED as an opt-in (`EarlyAnalysisConfig::ENABLED`, default stays off: the
benefit depends on the STT final arriving fast, the waste ratio on conversational speech is not
measured yet, and the sample is six stops). Recommended settings: `fall_delay` 64 ms with the
default 100 ms `min_silence`.

## Voice-to-voice benchmark over the websocket
`brisvox-bench v2v` (code: `brisvox_bench::v2v`; options, JSON format and definitions in
`bench/results/README.md`) measures the V2V of the definitions above from the outside: a client
connects to any server that speaks `docs/WEBSOCKET_PROTOCOL.md`, streams a fixture in real time
and takes the arrival of the first non-silent bot audio frame after each ground-truth end of
speech (fixture sidecar) minus that end of speech. Percentiles (nearest rank p50/p90/p99)
are over all answered turns of N runs, also split by fixture, utterance language and turn
position; turns that are not clean samples (`talked_over`, `bot_speaking`, `no_response`) and
failed calls are counted, never silently dropped, and a `talked_over` or `no_response` turn (a
reply slower than the gap or the response window) makes the result unfit to commit in live mode
too, because percentiles without the slowest turns would understate the tail. After the fixture
the client keeps the line open with digital silence until the last reply, so servers that count
end-of-turn silence from incoming frames are measured fairly. Output: markdown (committed under
`bench/results/`) and JSON with every sample.

- **Same client for every system.** It only sees a URL, so the engine, the Pipecat baseline
  (behind a websocket adapter) and a mock are measured identically. Live mode takes the
  URL from the command line or `BRISVOX_BENCH_URL`; the harness holds no provider keys, and a URL
  with a token is redacted everywhere it could be printed.
- **Pipecat baseline**: `bench/pipecat_baseline/` is a Pipecat 1.12.0 bot (pinned,
  Python 3.13) behind a brisvox-protocol adapter. It reads the same agent file as brisvox-server
  (`bench/pipecat_baseline/agent.toml`: Deepgram nova-3, OpenAI Responses `gpt-6-luna`, Cartesia
  `sonic-3.6`, the same voice, Silero v6.2.3 and the same Smart Turn v3.2 file); its README has
  the brisvox -> Pipecat settings table, the known differences and the exact live commands. Its
  `--fake` mode (no keys) is the Pipecat-side self-check: 423-457 ms over a ~400 ms floor
  (Windows, 2026-10-13). Its `--mock-providers` mode runs the real pipeline over mocks
  with the brisvox mocks' latencies: the capacity comparison is "Capacity benchmark" below. The
  live-provider V2V comparison is the live phase.
- **Sim mode measures the engine**: every call runs the production assembly
  `brisvox_engine::run_call` behind the real websocket transport, in process, with its heavy work
  on a real inference pool; only the providers are `brisvox_sim` mocks with fixed latencies
  (`brisvox_bench::v2v::engine_target`). Not production: the VAD is the simulator's energy
  detector (run by the production VAD worker) and the Smart Turn model always answers "complete".
  Each turn therefore has a known floor, stated in every report:
  - **end of turn** = VAD stop + STT final (the turn driver holds the end for the final), or the
    fixed silence of `--engine-turn fixed:MS` if that is longer (counted from the backdated stop);
  - **latency floor** = end of turn + LLM first token + TTS first byte: 300 + 150 + 350 + 150 =
    **950 ms** with the defaults. The answer (`Okay, got it.`) comes in one token and its first
    clause (`Okay,`) is a first TTS chunk of its own (one word), decided inside the token.
    Before the first-chunk tuning the first chunk needed three words, waited one token gap (30 ms) for the
    response's end, and the floor was 980 ms.

  V2V minus the floor is the internal overhead of this document, measured at the client and
  including the transport: the engine's own time (VAD windows, pool hops, stage queues, the
  20 ms frame grid, the output pacer's phase) plus the websocket and the harness.
  `--target mock-bot` keeps the original mock bot (energy end-of-turn after `endpoint` of silence, then
  `delay`, floor `endpoint + delay`) as a self-check: its excess over the floor is the harness and
  transport share alone.

  Measured before the pacer phase was turned off by default (the engine row carries it on its first turn per call), 2026-10-13 (engine) and 2026-10-12 (mock bot) (`bench/results/v2v-sim.md`,
  `v2v-mock-bot.md`; 10 runs of each of the three fixtures, 60 latency turns, release build,
  **Windows** dev machine shared with other builds, so trends only: the comparisons of
  the Pipecat and LiveKit comparisons need a Linux run, which is still to be recorded):

  | target | floor | p50 | p90 | p99 (= max) | excess over the floor p50 / p90 / p99 |
  |---|---:|---:|---:|---:|---:|
  | engine over mock providers | 950 ms | 1010 ms | 1030 ms | 1058 ms | 60 / 80 / 108 ms |
  | mock bot (harness + transport) | 800 ms | 828 ms | 846 ms | 858 ms | 28 / 46 / 58 ms |

  The engine's share is the difference of the two excess columns, about 32 / 34 / 50 ms (p50 /
  p90 / p99) on top of the harness and transport, and the engine's own timeline shows what it
  is made of ("Engine stage times and memory" below): the stages themselves cost well under a
  millisecond; the rest is the mock providers' sleeps overshooting on the Windows timer (about
  8 ms each for the LLM's first token and the TTS's first byte at p50, 17 ms at p95), the VAD
  windows (32 ms) and the 20 ms frame grid with the output pacer's phase at both ends. The
  largest client send lateness was 26 ms (Windows timer), which lands in both columns. The
  2026-10-12 engine run (980 ms floor: the first chunk then needed three words) measured
  1036 / 1062 / 1081 ms.
- **Harness fidelity.** Frames go out at absolute deadlines (`begin + k * 20 ms`, a frame once its
  last sample has been spoken); the largest send lateness is in every report. A late final speech
  frame adds its lateness to that turn's V2V (Windows timer tick ~15.6 ms; Linux ~1 ms), so
  comparisons use Linux runs.
- The measured frame is the one that *arrives*: transport buffering at the server's output (the
  pacer's phase offset of up to one frame until it was removed from the default) is part
  of V2V, as the definition requires.

## Live comparison with real providers: brisvox vs the Pipecat baseline (measured 2026-10-13, Windows)

*Measured before the pacer phase was turned off by default: brisvox's first turn of each call includes the pacer phase (0-19 ms, mean 9.5 ms); the numbers were not re-measured.*

The first V2V comparison on real providers, from this Windows machine over the public internet
(raw reports with the environment banner, every turn and the failure counts:
`bench/results/v2v-live-{brisvox,pipecat}-windows-2026-10-13.{md,json}`, Turkish:
`...-tr-windows-...`; the reports are generated by `scripts/bench_live_report.py` from the bench's
own reports and the systems' logs). Both systems ran the same agent file
(`bench/pipecat_baseline/agent_live_en.toml`): Deepgram `nova-3`, `openai/gpt-6-luna` through
OpenRouter (Chat Completions, streaming, `reasoning_effort` none, one-sentence prompt), Cartesia
`sonic-3.6`, adaptive turns with the same Smart Turn v3.2 file and Silero v6.2.3, one system at a
time, `brisvox-bench v2v --min-gap-ms 9000` (the default 4 s gap is shorter than a real answer:
the first reply would be cut by the second utterance).

| English, V2V (ms) | turns | p50 | p90 | p95 | max |
|---|---:|---:|---:|---:|---:|
| brisvox, first 3 calls (as specified) | 6 | 1580 | 2102 | 2102 | 2102 |
| Pipecat, first 3 calls (as specified) | 6 | 2173 | 2588 | 2588 | 2588 |
| brisvox, 15 calls pooled | 30 | 1686 | 2203 | 2927 | 4287 |
| Pipecat, 15 calls pooled | 30 | 2029 | 2370 | 2544 | 2588 |
| brisvox, Turkish, 3 calls | 6 | 1994 | 3031 | 3031 | 3031 |
| Pipecat, Turkish, 3 calls | 6 | 2519 | 3477 | 3477 | 3477 |

The 15 calls are three blocks per system (3 + 6 + 6 calls), alternating brisvox and Pipecat; the
first block is the run as specified, the others were added because the LLM provider's variance is as
large as the effect. All turns were answered, no call failed, no `no_response`, `talked_over` or
`bot_speaking` turn on either side.

Provider latencies as each system's own timers saw them (English, 30 turns each, p50 / p90 / p95 ms;
brisvox: the `turn finished` timeline of the server, Pipecat: its service TTFB timers):

| stage | brisvox | Pipecat |
|---|---:|---:|
| STT final after the end of speech | 285 / 319 / 320 | 420 / 460 / 471 (TTFB 320 / 360 / 371 from the VAD stop, plus its 100 ms) |
| LLM first token (OpenRouter included) | 741 / 1423 / 2229 (max 3178) | 1064 / 1328 / 1454 (max 1884) |
| TTS first audio (Cartesia) | 212 / 231 / 250 | 211 / 229 / 244 |

What the 343 ms p50 difference (1686 vs 2029) is made of:

- **STT final, about 135 ms in brisvox's favour.** brisvox hands Deepgram a `Finalize` at the VAD stop
; Pipecat waits for the provider's own endpointing (300 ms of silence) and its final.
  This is a framework behaviour, not provider variance.
- **LLM first token, about 320 ms in brisvox's favour, mostly not attributable to the frameworks.**
  Both call the same OpenRouter model with the same body. Interleaved A/B runs of the request
  (raw httpx against Pipecat's `OpenAILLMService` client, 14 pairs, warm connection) gave the same
  median (1.03 s and 1.06 s): Pipecat's client adds nothing over a plain HTTP/1.1 client, and the
  provider's time swung between 0.63 s and 3 s for identical requests within the day. A third
  arm over HTTP/2 (what reqwest negotiates for brisvox) was about 170 ms lower at the median in a
  second, noisy A/B, a plausible part of the gap; the rest is within the provider's variance. The
  p95 of the LLM is higher for brisvox (a few slow responses, 3.2 s and 2.2 s, that landed in
  brisvox's blocks): the tail of the V2V columns is the provider's, not a framework's.
- **TTS first audio: no difference.** Same Cartesia endpoint, same 24 kHz websocket.
- **The framework's own share.** brisvox's server-side response latency equals its STT final plus
  its LLM up to the first speakable chunk plus its TTS first byte to within a few hundred
  microseconds (the engine hand-offs; "of which inside the server": 0 ms in the report). What is
  left of V2V after the three providers (about 220 ms at p50) is client side: the VAD's end of
  speech against the fixture's ground truth, the 20 ms frame grid, the websocket. Pipecat's residual
  in the same definition (V2V minus the 100 ms VAD stop, STT, LLM first token and TTS first byte) is
  308 ms at p50, brisvox's with the identical definition 400 ms: brisvox's is larger because it also
  contains the wait for the first speakable chunk (195 ms at p50 after the first token), which in
  Pipecat is hidden in its own sentence aggregation. The frameworks' overheads are close; the
  measured gap comes from the STT finalisation and from the LLM client/provider path.
- **Pipecat restarted the LLM in 5 of 30 English and 4 of 6 Turkish turns.** Its turn-stop
  strategy ended the turn at a mid-sentence pause and started the LLM, and the user's resumed
  speech cancelled it. The V2V of those turns is still measured from the end of the utterance, but
  the user was cut off once; brisvox made exactly one LLM request per turn.

Caveats, read before quoting any number:

- **Network variance and sample size.** 30 turns per system and 6 for Turkish, 2 turns per call,
  one machine, one afternoon; p90 and p95 are the 3rd and 2nd largest samples. OpenRouter sits in the
  path of both systems and its first-token time varied 5x for the same request. Do not read the
  p95 columns as tail estimates.
- **Windows, not Linux.** The timer tick adds up to ~15 ms of send lateness (in each raw report) and
  the machine was shared with other work (a Docker/WSL benchmark in another session).
- **IPv6.** This machine's IPv6 route drops packets. Pipecat 1.12.0 as shipped connects its
  websockets one address at a time: the first Cartesia handshake of a call timed out after 10 s and
  the first turn got no audio (observed in the first attempt). The baseline bot therefore enables
  asyncio's happy-eyeballs racing (`bot.py --no-happy-eyeballs` restores Pipecat as shipped); the
  numbers above are with it on. On a host with working IPv6 this does not matter.
- **Reasoning.** `openai/gpt-6-luna` thinks by default (about +1 s to the first token, ~20 reasoning
  tokens for a one-line answer); both systems send `reasoning_effort: none` (agent file key
  `[llm.openai_compatible].reasoning_effort`, added for this run).
- **Different timers.** The STT and LLM columns are each system's own measurements with their own
  start events (VAD-backdated end of speech vs the VAD stop frame, request written vs LLM service
  start); only V2V is measured identically, by the same client. Turkish transcription quality was not
  judged, only the latency of an answer.
- **Method notes.** A first pass with the bench's default 4 s gap was discarded (replies were cut
  by the next utterance); one Pipecat block was re-run because a stale bot instance had answered
  and no metrics were logged. About USD 0.003 of OpenRouter credit was used in all.

### Live comparison with 5 concurrent calls, 100 turns per system, Deepgram Aura voice (measured 2026-10-14, Windows)

*Measured before the pacer phase was turned off by default: brisvox's first turn of each call includes the pacer phase (0-19 ms, mean 9.5 ms); the numbers were not re-measured.*

The larger rerun of the comparison above (the 30-turn run stays as the earlier run, with Cartesia, one
call at a time). Raw reports with every turn, the failure counts and the routing check:
`bench/results/v2v-live-{brisvox,pipecat}-c5-windows-2026-10-14.{md,json}`, produced by
`scripts/bench_live_report.py` from the bench's reports and the systems' logs. English only.

Method:

- **Blocks.** 10 blocks per system, each `brisvox-bench v2v --concurrency 5 --runs 5` (5 calls at
  once, 2 turns per call, `--min-gap-ms 9000`), strictly alternating b1 p1 b2 p2 ... so both systems
  saw the same provider conditions; one warm-up block per system first (not counted). Both servers
  ran side by side and idle between their blocks; the client waited for each block to end. 100 turns
  per system, 200 plus 20 warm-up turns in all. Every call plays the same fixture, so the 5 calls
  speak at nearly the same moment and the providers see 5 simultaneous requests per block.
- **Same providers, same route.** Deepgram `nova-3` (STT), OpenRouter `openai/gpt-6-luna` (Chat
  Completions, streaming, `max_tokens` 120, `reasoning_effort` `none`, the lowest this model accepts:
  its `supported_efforts` are max, xhigh, high, medium, low, none), upstream pinned with
  `{"provider": {"order": ["openai"], "allow_fallbacks": false}}` (`openai` is the model vendor's own
  standard-tier endpoint), Deepgram Aura `aura-2-thalia-en` (TTS). Agent file
  `bench/pipecat_baseline/agent_live_en_aura.toml`, both systems. Smart Turn v3.2 (the same file) and
  Silero v6.2.3. Cartesia was the first choice and was dropped: its plan on this account allows 2
  concurrent generations (below).
- **Aura on both sides.** brisvox: its `deepgram_aura` adapter, `wss://api.deepgram.com/v1/speak`,
  `encoding=linear16&sample_rate=24000&model=aura-2-thalia-en`, a `Speak` per text chunk and one
  `Flush` per turn. Pipecat: `DeepgramTTSService`, the same URL and parameters, a `Speak` per sentence
  and a `Flush` when the LLM response ends. The reply is one sentence, so both send one `Speak` and
  one `Flush` and Aura starts synthesising at the flush. Differences that remain: brisvox's text
  chunking and its stall deadline; Pipecat's TTS TTFB timer starts at its `Speak`. Neither sends
  `speed`.
- **The upstream was checked per response.** brisvox logs `llm upstream` (the `provider` and `model`
  fields of the first stream chunk), the Pipecat baseline `PCUPSTREAM`. brisvox: 100 of 100 requests
  reported upstream OpenAI, model `openai/gpt-6-luna`. Pipecat: 102 responses, all OpenAI and the
  same model; it made 167 LLM requests in all (below). With a nonexistent slug pinned, both
  systems got OpenRouter's HTTP 404 on every request (checked on the Cartesia attempt), so the pin is
  really sent by both and is never silently replaced by a fallback.

Voice-to-voice (V2V, ms; same client, same definition; nearest rank over the 100 answered turns):

| English, 5 concurrent calls | turns | p50 | p90 | p95 | p99 | max | mean |
|---|---:|---:|---:|---:|---:|---:|---:|
| brisvox | 100 | 1846 | 2209 | 2876 | 5199 | 5235 | 1937 |
| Pipecat | 100 | 1972 | 2980 | 4348 | 6141 | 7529 | 2343 |

All 200 turns were answered; no failed call, `no_response`, `talked_over` or `bot_speaking` turn, no
error or warning line, no provider refusal on either side. The p50 of each block ranged 1566 to 1935
ms for brisvox and 1845 to 2167 ms for Pipecat (listed in the reports).

Provider latencies as each system's own timers saw them (100 turns each; p50 / p90 / p95 / p99 ms;
brisvox: the `turn finished` timeline of the server, Pipecat: its service TTFB timers):

| stage | brisvox | Pipecat |
|---|---:|---:|
| STT final after the end of speech | 300 / 337 / 363 / 376 | 455 / 499 / 505 / 521 (TTFB 355 / 399 / 405 / 421 from the VAD stop, plus its 100 ms) |
| LLM first token (OpenRouter included) | 931 / 1311 / 1984 / 4282 | 889 / 1381 / 3483 / 3892 |
| TTS first audio (Deepgram Aura) | 220 / 395 / 416 / 432 | 251 / 259 / 265 / 270 |
| V2V minus (STT final + LLM first token + TTS first audio; Pipecat: plus the 100 ms VAD stop) | 300 / 556 / 681 | 398 / 1143 / 1328 |

What the numbers say, and what they do not:

- **Medians are close, tails are not.** The p50 gap is 126 ms (1846 vs 1972); at p95 brisvox is
  1.5 s lower (2876 vs 4348). LLM first-token p50 is the same within the provider's noise (931 vs
  889 ms); the tail of the LLM column is the provider's (slow responses of 3 to 4.5 s land in both).
  The STT stage is a framework difference as before: brisvox's `Finalize` at the VAD stop gives a
  final 155 ms after the end of speech earlier at p50 than Pipecat's endpointing wait.
- **Pipecat restarted the LLM in 56 of 100 turns** (65 requests were cancelled before their first
  chunk; the 2026-10-13 run had 5 of 30; 39 of the restarts were on the first turn of a call, 17 on
  the second). The pattern is consistent with a turn ended at a pause inside the utterance whose
  LLM request was then cancelled by the resumed speech (no bot audio was produced for the cancelled
  requests); the cause was not investigated. A configuration difference is a candidate: the same
  nominal 100 ms VAD stop is 3 Silero windows (96 ms) in Pipecat and 4 (128 ms) in brisvox
  ("Unloaded floor" above), and this run did not equalise it (`--vad-stop-ms 128`). Its V2V
  p50 is 2351 ms (n = 56) for the restarted turns and **1791 ms (n = 44) for the turns without a
  restart**, close to brisvox's overall median (a selection effect too: the restarted turns are
  mostly first turns of a call). Why the restarts are more frequent than at one call at a time was not investigated (the
  baseline runs one Python process with Pipecat's default of one Smart Turn thread per call; five
  pipelines share one core's worth of GIL): unverified, not tuned. brisvox made exactly one LLM
  request per turn.
- **TTS first audio is not a difference.** Aura's first audio is 220 vs 251 ms at p50; brisvox has a few
  slower first audios (p90 395 ms, max 720 ms), Pipecat's TTFB is flat. The two timers start at different
  events (see the method).
- **The residual** (V2V minus the three provider stages) is 300 ms at p50 for brisvox and 398 ms for
  Pipecat in the common definition; client side, the VAD against the fixture's ground truth, the
  20 ms frame grid and the websocket are in it for both (the server's own overhead for brisvox is 0 ms
  to within the engine hand-offs, as before).

Caveats, read before quoting any number:

- Windows, one shared machine over the public internet, largest client send lateness 29.9 ms; the
  machine was shared with another session (no build ran during the blocks). Not a Linux run (`docs/LATENCY.md` asks
  for Linux numbers for target claims).
- 100 turns per system: p99 is the 2nd largest of 100 values, not a tail estimate
  with confidence; the p95 columns move with two or three slow provider responses.
- Five calls playing the same fixture speak in lockstep; real traffic is spread out. This stresses
  the providers' concurrent request handling more than a random mix would.
- brisvox runs one server process, the Pipecat baseline one Python process (the capacity benchmark
  uses 4 workers for Pipecat; here, 5 calls, one is what the baseline README documents).
- Cost: about USD 1 in all by list price, mostly Deepgram STT and Aura (OpenRouter well under USD
  0.05); not read from the invoices.
- Voice quality was not judged; Aura `aura-2-thalia-en` is not the Cartesia voice of the earlier run,
  so the TTS column is not comparable with that run.

#### Earlier attempt on Cartesia: stopped by its concurrency limit (2026-10-14)


The larger rerun (100 turns per system, blocks of 5 concurrent calls x 2 turns, brisvox and Pipecat
alternating, both pinned to the same OpenRouter upstream) was **stopped after one warm-up block per
system**: the Cartesia plan of this account allows **2 concurrent generations** ("You have reached
your concurrency limit of 2"), and five calls that answer at the same moment exceed it. brisvox's
Cartesia adapter re-opens a refused context after 300 ms (up to four times), so its 10 turns were
all answered but four of them carry 0.7 to 2.6 s of Cartesia waiting (`tts_ttfb`); Pipecat as
shipped surfaced 12 error frames and answered 7 of 10 turns (one whole call got no audio). The two
systems no longer do the same work at this concurrency, so no table was published for Cartesia (the
run above uses Deepgram Aura instead): raw numbers and
the routing checks are in `bench/results/v2v-live-c5-cartesia-limit-windows-2026-10-14.md`. A
Cartesia rerun would need a plan with a concurrency of at least 5, or `--concurrency 2`.

What this work added, usable for any later live run:

- `brisvox-bench v2v --url ... --concurrency N` plays the plan's calls N at a time (live mode
  only; reports carry `settings.concurrency`).
- Pinned LLM route on both systems. Agent file keys `[llm.openai_compatible] openrouter_order`
  and `openrouter_allow_fallbacks` send OpenRouter's `provider` routing object
  (`{"order": ["openai"], "allow_fallbacks": false}`); the Pipecat baseline maps the same keys
  into `extra_body` of its OpenAI client. Model `openai/gpt-6-luna`, `reasoning_effort` `none`
  (the lowest the model accepts: max, xhigh, high, medium, low, none), upstream `openai` (the
  model vendor's own standard-tier endpoint). Verified on the wire: both systems log which upstream
  the first stream chunk names (brisvox `llm upstream` with `upstream_provider`, the baseline
  `PCUPSTREAM`), and with a slug that cannot be served both get OpenRouter's HTTP 404 instead of
  a fallback.

## Engine stage times and memory (measured 2026-10-13)
`brisvox-bench v2v --sim` also reports the engine's own per-stage times (`engine_stages`; code
`brisvox_bench::v2v::stages`): every call runs on a `TimelineRecorder` of its own, and when it
ends its turns are collected. The stamps are taken where the work happens, on the engine's clock,
so the transport and the harness are not in them. Stages and how the turns feed them are in
`bench/results/README.md`; the barge-in calls (the caller talks over a long answer) give the
clear, the memory calls (8 at once) the memory. The engine overhead budget of
`docs/PIPELINE.md` section 8 applies at p95. With `--strict` a stage over its budget fails the run
(no JSON is written; `BenchReport::budget_problems`); without it the miss is a warning.

Release build, Windows dev machine shared with other builds, 60 latency turns and 30 barge-ins
(`bench/results/v2v-sim.md`):

| stage | from -> to | p50 | p95 | max | provider time in it | budget p95 |
|---|---|---:|---:|---:|---:|---|
| `vad_stop_to_finalize` | `VadStop` -> `SttFinalize` | 0.006 ms | 0.015 ms | 0.017 ms | 0 | - |
| `turn_end_to_llm_request` | `TurnEnd` -> `LlmRequest` | 0.051 ms | 0.089 ms | 0.212 ms | 0 | **3 ms: met** |
| `llm_request_to_first_token` | `LlmRequest` -> `LlmFirstToken` | 358.4 ms | 366.6 ms | 367.7 ms | 350 ms | - |
| `first_token_to_tts_text` | `LlmFirstToken` -> `TtsRequest` | 0.025 ms | 0.051 ms | 0.089 ms | 0 | - |
| `tts_request_to_first_byte` | `TtsRequest` -> `TtsFirstByte` | 158.4 ms | 166.7 ms | 167.1 ms | 150 ms | - |
| `tts_audio_to_transport` | `TtsFirstByte` -> `FirstAudioOut` | 0.035 ms | 0.064 ms | 0.103 ms | 0 | **2 ms: met** |
| `interrupt_to_clear` | `Interrupted` -> `AudioOutStopped` | 0.012 ms | 0.022 ms | 0.669 ms | 0 | **1 ms: met** |
| `turn_end_to_first_audio` | `TurnEnd` -> `FirstAudioOut` | 517.7 ms | 533.2 ms | 533.4 ms | 500 ms | - |

- **All three budgets are met**, by one to two orders of magnitude: the end of turn reaches the
  LLM in 0.09 ms at p95, the first TTS audio reaches the transport in 0.06 ms, a confirmed
  barge-in's `Clear` is applied in 0.02 ms (the worst of 30, 0.67 ms, is one scheduling delay).
- **The mocks' sleeps are not the engine's.** The two provider spans overshoot their fixed
  350 ms and 150 ms by 8.4 ms at p50 and 16.6 ms at p95 each: the Windows timer's precision
  (one ~15.6 ms tick at worst). `turn_end_to_first_audio` (17.7 ms over its 500 ms at p50) is
  those two overshoots plus the engine stages above, about 1 ms.
- **The span stamps.** `VadStop` is the VAD worker's decision (not the backdated
  `UserSpeechEnd`), `SttFinalize` the `Finalize` handed to the provider session (both marked with
  `TimelineRecorder::mark_open_turn`); `AudioOutStopped` is marked by the turn driver when its
  fast-path `Clear` returns, that is applied in the transport.
- **Memory.** The process (engine, transport and harness) holds 10.2 MiB with the server idle and
  cold, 22.1 MiB idle after the latency and barge-in calls (+11.9 MiB kept after the first calls:
  allocator caches and lazily built state) and peaks at 29.7 MiB with 8
  calls alive at once: **about 1.0 MiB per call** (peak over the warm idle reading, per call;
  measured on calls alive together, because one call at a time reuses what the previous one
  freed). The Smart Turn ring (8 s of 16 kHz PCM16, 256 KB) and its analysis snapshot (another
  256 KB) are about half of it; the capacity benchmark measures it at hundreds of calls.
- Linux numbers are still to be recorded for this table; on Linux the timer overshoot of
  the mocks shrinks to about a millisecond.

## Simulator Budget numbers (measured 2026-10-12)
The Budget profile of `brisvox_sim::suite` (table in the `suite` module docs) was set from what
the production pipeline (`EngineFactory`, `engine_suite.rs`) and the hybrid chains
(`hybrid_*.rs`, `speculation_hybrid.rs`) measure on a paused clock with zero-latency transport
and the suite's mock latencies. Paused time: these are pipeline-structure numbers (turn policy,
pacer phase, frame grid), not CPU cost; the engine's own overhead is measured in real time by the
V2V bench. Measured by temporarily logging every `Profile::duration`/`at_least` value over
all engine tests (the hook is not in the tree):

| check | reference (Exact) | measured, production + hybrids | Budget |
|---|---|---|---|
| one turn: first bot audio | 1480 ms | 1480, 1495 (adaptive, prod.), 1650, 1665 (fixed 450 ms, prod.); since the phase was turned off, prod. is 1480 and 1650 | 1480 to 1700 ms |
| speech start / end marks | 200 / 1200 ms | 200 / 1200 ms | within 20 ms |
| end-of-turn delay | 280 ms | 280 (adaptive), 450 (fixed) | 280 to 470 ms |
| response latency | 280 ms | 280 (adaptive), 450 (fixed) | 280 to 470 ms |
| s01 first audio after the end of speech | 580 ms (min) | 1008 to 1079 ms | at least 580 ms |
| barge-in: interrupt at the output | 2420 ms | 2420 ms | 2400 to 2440 ms |
| barge-in / s02: time to silence | 0 | 0 | 0 to 50 ms (kept for a slow consumer) |
| s05 LLM first token behind a 2 s stall | 2000 ms | 2000 ms | 2000 to 2020 ms |
| playout log vs frames at the output | equal | 0 (677 checks) | within 20 ms |
| fixtures: first audio after each utterance end | at most 600 ms | 280 to 305 (adaptive), 450 to 475 (fixed) | at most 600 ms |

- The +15 ms of the production first audio over the hybrids (1495 against 1480) was the transport
  pacer's per-session phase offset: the first frame waited for the session's next 20 ms tick. Now
  the first frame leaves at once and the production pipeline measures 1480 ms like the
  hybrids (the phase is an opt-in of the pacer; see "The pacer phase" below).
- The widest range is the fixed 450 ms turn end (170 ms over the reference's 280 ms backdated VAD
  stop); the adaptive policy with a confident model ends the turn at the stop like the reference.
- Every range keeps the reference's exact value (`seam.rs`, `the_budget_profile_accepts_the_
  reference_pipeline`) and one 20 ms frame of margin over the largest measured value.

## Hot-path micro-benches (measured 2026-10-12)
`crates/brisvox-engine/tests/hot_path_cost.rs`, release build
(`cargo test --release -p brisvox-engine --test hot_path_cost -- --nocapture`), Windows dev
machine shared with other builds; single calls, best of three runs per statistic. Every case
that runs on a tokio worker is far inside the 50 us rule; the test asserts the median against
50 us in release (10 ms in the debug gate).

| Case | Median | p99 | Worst | On a worker |
|---|---|---|---|---|
| `Segmenter::push_spans` (4-12 byte delta) | 0.1 us | 0.4 us | 2.4 us | yes |
| `ResponseAssembler::push` (text delta) | 0.0 us | 0.1 us | 0.4 us | yes |
| `ResponseAssembler::finish` (2 KB, one call) | 1.6 us | 1.6 us | 3.6 us | yes |
| `ResponseAssembler::finish` (4 KB = `inline_finish_bytes`, one call) | 2.5 us | 2.6 us | 4.7 us | yes |
| `PlayedText::heard` (200 words) | 4.5 us | 4.7 us | 11.5 us | yes |
| `TurnMachine::handle` (a turn's inputs) | 0.0 us | 0.1 us | 0.3 us | yes |
| `StreamResampler` 8 kHz -> 16 kHz, 20 ms frame | 0.6 us | 0.7 us | 8.4 us | yes |
| `StreamResampler` 48 kHz -> 16 kHz, 20 ms frame | 1.8 us | 1.8 us | 4.3 us | yes |
| `OutputFramer` 24 kHz -> 16 kHz, 20 ms chunk | 1.2 us | 1.3 us | 2.8 us | yes |
| `OutputFramer` 24 kHz -> 16 kHz, 100 ms chunk | 5.8 us | 6.0 us | 8.5 us | yes |
| `OutputFramer` 24 kHz -> 8 kHz, 100 ms chunk (the stage's piece) | 30.5 us | 33.0 us | 40.5 us | yes |
| `OutputFramer` 24 kHz -> 16 kHz, 500 ms chunk (the stage cuts it in 100 ms pieces) | 28.8 us | 31.3 us | 42.8 us | no |
| `RequestBuilder::build` (`build_window`), 20 turns | 1.3 us | 1.4 us | 1.4 us | yes |
| same, 380 messages (below `INLINE_MAX_MESSAGES`) | 2.3 us | 2.5 us | 4.8 us | yes |
| same, 4 x 3.7 KB results (below `INLINE_MAX_TURN_BYTES`) | 2.8 us | 3.7 us | 4.9 us | yes |
| `Context::clone`, 380 messages | 4.8 us | 4.9 us | 6.8 us | yes |
| `is_heavy`, 380 messages | 0.3 us | 0.3 us | 0.3 us | yes |

Nothing moved to the pool: the closest case is the 24 kHz to 8 kHz conversion of a 100 ms piece
(30 us), which the TTS stage already bounds by cutting provider chunks into such pieces with a
yield between them. The whole-context limit of `is_heavy` (`INLINE_MAX_CONTEXT_BYTES`, 256 KiB)
is set from the clone cost: a context copy shares message text (`Arc<str>`), so
it costs per message, not per byte, and 380 messages copy in 5 us; the byte limit catches the
JSON values (tool arguments, provider data) that a copy does duplicate.

### 8 kHz mu-law call output path (measured 2026-10-13)
`crates/brisvox-engine/tests/ws_mulaw_call.rs`, release build
(`cargo test --release -p brisvox-engine --test ws_mulaw_call the_per_frame -- --nocapture`),
Windows dev machine shared with other builds, median of single calls, best of three runs. One
40 ms chunk of 24 kHz PCM16 through `OutputFramer` (24 kHz to 8 kHz) into two 160-sample frames,
then the transport's mu-law encode of a frame:

| Step, per 20 ms frame | Median |
|---|---|
| resample + frame | 6.65 us |
| mu-law encode | 0.03 us |
| together | 6.68 us (budget 50 us) |

The test asserts 50 us only in release; `scripts/verify.sh` is a debug run, so the gate holds
the 10 ms gross-regression bound there (the same policy as `hot_path_cost.rs`). Re-run the
command above and update this table when the resampler or the framer changes; a release perf
gate in `verify.sh` was not added (a release build of the workspace doubles the gate's time and
RAM).

Real-time scheduler gaps under load (20 calls, `realtime_load.rs`): release worst 1.67-2.61 ms
on the shared machine (at most two polls over 2 ms in a 14 s run): the 2 ms acceptance bound
is not met on this evidence (a quiet-machine run is still to be recorded), see `docs/TESTING.md`.

### First TTS chunk
The first TTS chunk of a response ends at the earliest clause boundary or sentence end once it
holds `first_min_words` words (`SegmenterStageConfig::FIRST_MIN_WORDS_EN` / `_TR`). A voice reply
typically opens with a one-word clause (`Sure,` `Okay,` `Peki,`) and then a long sentence: with two
or three words the first chunk waits for that whole sentence, streamed token by token. Earlier
the defaults were three (English) and two (Turkish): the chaos suite's LLM, which streams
`Sure, here is a fairly long answer to your question.` word by word 30 ms apart, was spoken only
when the sentence was complete (TTS request 300 ms after the first token on the paused clock).

Measured on the production pipeline (`crates/brisvox-engine/tests/first_chunk_policy.rs`): the
chaos suite's 100 unaffected reference calls (seeds 2000..2099, two turns each, played alone;
LLM first token log-normal with a 350 ms median, 30 ms per token; TTS first byte log-normal
150 ms; adaptive end of turn), first audio after the end of speech:

| first chunk after | p50 | p95 | max | turns over the 1.5 s SLO |
|---|---:|---:|---:|---:|
| 1 word | 969 ms | 1262 ms | 1366 ms | 0 of 200 |
| 2 words | 1137 ms | 1485 ms | 1592 ms | 9 of 200 |
| 3 words | 1137 ms | 1485 ms | 1592 ms | 9 of 200 |

The default is now **one word in both languages**. English is measured: 168 ms off the median of
this answer shape,
and the 100-call chaos test passes with the production default and the unchanged 1.5 s SLO (the
`with_first_chunk_words(1)` override is gone). The TTS keeps prosody across chunks because every
chunk of a round goes into the same provider context; whether a one-word first chunk
sounds natural with each real TTS is checked with the live tests. **Turkish is an
assumption, not a measurement**: no Turkish answer is measured here, and one word is assumed
because a Turkish reply opens the same way (`Peki,` `Tamam,` and a long sentence). A Turkish
reference set (or the live Turkish TTS tests) still has to confirm it, prosody included.
`SessionBuilder::with_first_chunk_words` still overrides it per call.

### Live prosody and speculation checks (measured 2026-10-13)
One-word first chunks (`Okay,` / `Tamam,` + the rest 150 ms later, one context, real providers):
first audio 120-220 ms on Cartesia and ElevenLabs (the same as one chunk), no pacer underrun, no
pause beyond the natural comma pause on Cartesia; ElevenLabs Turkish adds 210-230 ms; on Deepgram Aura-2 the
one-word chunk gives no first-audio gain (413-418 ms vs 257-271 ms for one chunk). FIRST_MIN_WORDS
is unchanged. With Deepgram nova-3 the partial at the VAD stop equals the final in 2 of 17 turns
(interims arrive once per second, the finalize round trip is 290-330 ms): `on_partial = false`
for Deepgram, EN and TR. Data: `bench/results/live-checks-2026-10-13.md`,
`bench/results/speculation-live-deepgram-2026-10-13.md`.

## Agent link hop (measured 2026-10-13)
The out-of-process agent protocol (`docs/AGENT_PROTOCOL.md`) adds one hop when the agent reacts
to the call: an engine event leaves the call, crosses the WebSocket to the agent process, the
agent answers with a command naming the event (`re`), and the server parses the command. The
link measures every such hop itself, from the moment its event pump took the event from the
call's event ring to the moment the reader parsed the command: `LinkStats::hops`,
`hop_mean()`, `hop_max_us` and a bucketed histogram (`hop_quantile(q)` gives the bucket's upper
bound; buckets 50, 100, 200, 300, 500, 750 us, 1, 1.5, 2, 3, 5, 10, 20, 50, 100 ms).

Measured by `crates/brisvox-agent-link/tests/hop_latency.rs` (500 hops; the reference agent in
Rust answers each `user.text` at once with a command; real sockets on 127.0.0.1; Windows 11,
debug build, `cargo nextest run -p brisvox-agent-link --test hop_latency --no-capture`):

| pattern | mean | p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| back to back (the next event leaves when the agent answered) | 0.08-0.11 ms | <= 0.1 ms | <= 0.3 ms | <= 0.3 ms | 0.23-0.37 ms |
| paced, one event every 2 ms (threads idle between events; Windows timer: ~15 ms steps) | 0.38-0.43 ms | <= 0.5 ms | <= 0.75 ms | <= 0.75 ms | 0.71-0.97 ms |

**Release build (measured 2026-10-13)**, the same test, `cargo nextest run --release -p
brisvox-agent-link --test hop_latency --no-capture` (back to back, 500 hops, Windows 11):

| build | mean | p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| release | 0.033 ms | <= 0.05 ms | <= 0.1 ms | <= 0.2 ms | 0.221 ms |

The hop is dominated by two loopback socket writes and the scheduler. The debug table above predates
the agent link's reconnect work (the link's tasks were restructured for reconnecting); only the release run was repeated.

So on loopback the hop costs well under a millisecond, far below one 20 ms audio frame and the
provider latencies; an agent elsewhere adds its network round trip and its own processing on
top. A release build only lowers these numbers. The commands themselves act at the engine's
next decision point: a `say` starts as soon as the turn driver handles its wake-up (the bot
idle), a `context.update` at the next response.

## Speculative LLM start (measured 2026-10-13)

The question: should `EngineConfig::speculation` (start the LLM at a candidate end of turn,
buffer its output, commit it if the turn ends with the same words) be on by default? **Decision:
no, it stays opt-in** (reasons below). This section is the decision text. The numbers behind it:

**What was run.**
- `brisvox-bench speculation` (`bench/results/speculation-fixtures.md`): the production engine
  (`EngineFactory`) over the three real-speech fixtures with mock providers on a **paused clock**,
  20 seeds per fixture, log-normal latencies around STT final 150 / LLM first token 350 / TTS first
  byte 150 ms (the same seeds in every row, so rows differ only by the configuration), played
  twice: with the LLM first token at 350 ms (after the miss is known) and at 100 ms (before it).
  Deterministic.
- `brisvox-bench v2v --sim --speculation ...` (`bench/results/v2v-speculation.md`, one file pair
  per row in `bench/results/v2v-speculation/`): the same engine behind the websocket, real time,
  fixed provider latencies, 16 calls x 2 turns per row. Windows numbers are trends (about 8 ms of
  timer overshoot per mock sleep), and p90/p99 over 32 turns are coarse.
- Turn policies: adaptive (Smart Turn model that answers "complete", the engine default), and
  fixed silences of 450 ms (= the finals gate, VAD stop 300 + STT final 150) and 700 ms.
- Speculation modes: off; on with `on_partial` and the STT's last interim **lagging** the final by
  a word (the mock's own interims: the worst case, every partial misses); on with the interim
  **converged** to the final (the best case: every partial hits); on **final only** (`on_partial`
  off: candidates only at the STT final).
- The benchmark needed three honest-measurement fixes: the mock LLM now answers by request
  (`MockLlm::set_responder`; with the queue, a discarded speculative request consumed the next
  turn's answer), the mock STT can converge its last interim to the final
  (`MockStt::set_converged_interims`), and it can time the interims from the speech instead of
  from the first frame after the previous `Finalize` (`set_interims_from_speech`; before, the
  second utterance of every call had its interims during the silence before it, so it never had a
  partial at the VAD stop). The V2V engine target uses both new STT settings and the responder
  for every row, speculation off included.

**Paused clock, fixtures (120 turns per row, V2V in ms, server side).**

LLM first token 350 ms (`bench/results/speculation-fixtures.md` has the same rows with the fast
LLM, the counters and the token columns):

| policy | speculation | V2V p50 | p90 | p99 | delta p50 | turns committed | run hit rate | head start ms | extra LLM requests per turn |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| adaptive | off | 923 | 1106 | 1168 | - | - | - | - | 0.00 |
| adaptive | on, lagging interims | 925 | 1113 | 1244 | +2 | 0% | 0% | - | 1.00 |
| adaptive | on, converged interims | 782 | 911 | 1013 | -141 | 100% | 100% | 154 | 0.00 |
| adaptive | on, final only | 923 | 1106 | 1168 | +0 | - | - | - | 0.00 |
| fixed 450 ms | off | 963 | 1106 | 1183 | - | - | - | - | 0.00 |
| fixed 450 ms | on, lagging interims | 925 | 1113 | 1244 | -38 | 72% | 42% | 38 | 1.00 |
| fixed 450 ms | on, converged interims | 782 | 911 | 1013 | -181 | 100% | 100% | 181 | 0.00 |
| fixed 450 ms | on, final only | 923 | 1106 | 1168 | -40 | 72% | 100% | 38 | 0.00 |
| fixed 700 ms | off | 1202 | 1331 | 1433 | - | - | - | - | 0.00 |
| fixed 700 ms | on, lagging interims | 925 | 1113 | 1244 | -277 | 100% | 50% | 266 | 1.00 |
| fixed 700 ms | on, converged interims | 857 | 954 | 1013 | -345 | 100% | 100% | 420 | 0.00 |
| fixed 700 ms | on, final only | 925 | 1106 | 1168 | -277 | 100% | 100% | 266 | 0.00 |

Two hit rates, not to be mixed up. `turns committed` is per turn: committed speculations over
answered turns. `run hit rate` is per run: committed over (committed + discarded runs). They agree
only with one candidate per turn. Fixed 700 ms with lagging interims starts two runs on every
turn (one from the partial at the VAD stop, superseded when the final arrives, and one at the
final that commits): 100% of the turns commit, 50% of the runs hit, and one request per turn is
wasted. The extra requests are `started - committed` per turn, read from the engine's counters.

**Same, with a fast LLM (first token 100 ms, before the STT final; paused clock, 120 turns per row).**

| policy | speculation | V2V p50 | delta p50 | extra LLM requests per turn | wasted output tokens per turn | wasted input tokens per turn (reported) |
|---|---|---:|---:|---:|---:|---:|
| adaptive | off | 677 | - | 0.00 | 0.00 | 0.0 |
| adaptive | on, lagging interims | 678 | +1 | 1.00 | 1.95 | 17.4 |
| adaptive | on, converged interims | 572 | -105 | 0.00 | 0.00 | 0.0 |
| fixed 700 ms | off | 950 | - | 0.00 | 0.00 | 0.0 |
| fixed 700 ms | on, lagging interims | 844 | -106 | 1.00 | 1.95 | 17.4 |
| fixed 700 ms | on, final only | 844 | -106 | 0.00 | 0.00 | 0.0 |

**Real time over the websocket (32 turns per row, V2V in ms, client side).**

| policy | speculation | V2V p50 | p90 | p99 | delta p50 | run hit rate | head start ms | extra LLM requests per turn |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| adaptive | off | 1005 | 1026 | 1047 | - | - | - | - |
| adaptive | on, lagging interims | 1003 | 1020 | 1036 | -3 | 0% | - | 1.00 |
| adaptive | on, converged interims | 849 | 875 | 887 | -156 | 100% | 155 | 0.00 |
| fixed 700 ms | off | 1263 | 1282 | 1297 | - | - | - | - |
| fixed 700 ms | on, lagging interims | 1011 | 1038 | 1055 | -252 | 50% | 253 | 1.00 |
| fixed 700 ms | on, converged interims | 903 | 921 | 938 | -361 | 100% | 410 | 0.00 |
| fixed 700 ms | on, final only | 1009 | 1032 | 1061 | -255 | 100% | 261 | 0.00 |
| fixed 450 ms | off | 1017 | 1042 | 1046 | - | - | - | - |
| fixed 450 ms | on, lagging interims | 1006 | 1020 | 1030 | -11 | 44% | 16 | 1.00 |
| fixed 450 ms | on, converged interims | 845 | 869 | 882 | -172 | 100% | 168 | 0.00 |
| fixed 450 ms | on, final only | 1002 | 1028 | 1043 | -14 | 100% | 15 | 0.00 |

"Extra LLM requests per turn" is the number of speculative runs that were not committed
(`started - committed`, per turn): each one is a request with a full prompt that was sent for
nothing. The engine also counts the output tokens a discarded run had buffered
(`wasted_output_tokens`) and the input tokens the provider reported for it. **Measured, with two
results.** With the 350 ms LLM both are 0: the first token comes after the miss is known (the STT
final, 150 ms) and the run is cancelled before it streams or reports usage; the real-time table
above is that case only, so its "0 wasted tokens" says nothing about a faster LLM. With the 100 ms
LLM the first token precedes the final, a discarded run streams into its buffer and the counters
are nonzero (1.95 output tokens and 17.4 reported input tokens per turn in the table above; the
scale is the mock's short scripted answers and short prompts, so read it as nonzero, not as a
price). The cost of a miss is its request plus whatever the run streamed before it was cancelled.

**Reading the tables.**
- **Adaptive (the default policy).** A complete verdict ends the turn at the STT final, so the
  only candidate that helps is the one from the partial at the VAD stop (`on_partial`). If the
  partial equals the final it saves one STT finalize round trip: -141 ms p50 on the paused clock,
  -156 ms in real time (about 15% of the 920-1000 ms V2V), at no cost. If the partial lags the
  final it saves nothing (+2 / -3 ms) and costs one extra request on **every** turn. `final only`
  never starts a run under this policy (identical to off).
- **Fixed silence longer than the gate (700 ms).** The final arrives at 450 ms but the turn ends at
  700 ms. A candidate at the final uses the 250 ms in between: -277 ms (paused) / -255 ms (real
  time) with no request wasted when the caller does not resume, and without any partial (`final
  only`). The partial adds a little more only if it converged (-345 / -361 ms) and doubles the
  requests if it lags.
- **Fixed silence equal to the gate (450 ms).** The final-based candidate arrives with the turn
  end: -40 / -14 ms. Only the partial helps (-181 / -172 ms), and only when converged.
- **No latency is lost on a miss.** The adaptive miss row is within 3 ms of off at p50 (p99 is 76
  ms higher on 120 paused-clock turns, 11 ms lower on 32 real-time turns: noise). A discarded run
  is cancelled and the real request starts at the turn end, as without speculation.
- The head start of a committed run is bounded by what the turn waits for after the request: the
  STT final (about 150 ms) for the partial, the silence beyond the final for the final-based run.

**Cost per call.** The cost is the extra requests, read from the counters: a call of `T` answered
turns whose requests carry `P` prompt tokens sends `E x T` extra requests, with `E = (started -
committed) / turns` the "extra LLM requests per turn" column, i.e. `E x T x P` extra input tokens,
**+E x 100% of the call's LLM input tokens** before prompt caching (the stable prefix of a wasted
request is a cache hit at the provider's cached rate, which lowers the money, not the count).
`E` is a property of the policy and the STT, from the tables: 0.00 with converged interims or
with speculation at the final under a fixed silence longer than the gate; 1.00 with lagging
interims in every policy measured (adaptive: one run per turn, all discarded; fixed 700 ms: two
runs per turn, one discarded, although every turn commits). For T = 10 and P = 1,500: 15,000 input
tokens per call without speculation, +0 at E = 0, +15,000 (double) at E = 1.00. With a single
candidate per turn (the adaptive policy, where only the partial can start a run) `E = 1 - h`
for `h` the turn hit rate, so +4,500 tokens at h = 70%; that shortcut does not hold when a turn
can start two runs. Output tokens: about 0 when the miss is known before the first token (the STT
final, 100-300 ms, usually precedes the first token, 200-500 ms) and nonzero otherwise (the
100 ms LLM row above); the engine counts them in `wasted_output_tokens`. Each miss is also one
more concurrent request against the provider's rate limit for up to the STT final's duration.
The money is `E x T x P x` the provider's input price, left symbolic on purpose (prices change;
this document does not quote them).

**Why it stays off.**
1. The gain under the default policy is at most the STT finalize round trip and exists only when
   the partial at the VAD stop already equals the final. Whether it does is a property of the STT
   provider and of the pause the caller makes, not of the engine; the fixtures bracket it between
   0% (cost with no gain) and 100%, and nothing in the repository measures a live STT's hit rate
   (Deepgram documents that the words at the end of an interim can be truncated, and that
   interims come typically every second, `docs/providers/deepgram.md`).
2. The 100% bound is not an upper bound for real calls: a pause of 300 ms or more inside a
   sentence (VAD stop, then the caller resumes) wastes a request however good the STT is. The
   fixtures have no such pause.
3. Default-on would double the LLM input tokens of every deployment whose STT lags, with no
   change in behaviour that anyone chose; that is a cost regression, not a latency feature.
4. A discarded speculative request consumes a queued answer of the scripted mock LLM, so every
   test that scripts a default pipeline by queue would have to move to a responder first.

**What to do instead.**
- Fixed silence of 600 ms or more: enable speculation with `on_partial = false`
  (`speculation.enabled = true`, `speculation.on_partial = false` in the server configuration).
  It costs no request when the caller does not resume and returns the silence beyond the final.
- Adaptive: enable `on_partial` per deployment once its STT's hit rate is measured (Deepgram
  measured 2026-10-13: 2 of 17 turns, so NOT for Deepgram, see the live checks below;
  AssemblyAI, Gladia: not measured yet, needs keys). Rule of thumb used here, for the
  adaptive policy (one candidate per turn): worth it from a turn hit rate of about 70%, i.e. at
  most +30% input tokens (`E` = 0.30) for about -150 ms. A hit rate below about 40% is a net loss
  (a judgement call, not a measurement).
- The stable-partial gate is below. Still open: offering the partial when the Smart
  Turn verdict is already "complete", which would raise the hit rate without more requests.

**Stable-partial gate (measured 2026-10-13).**

The follow-up of the section above: do not start a request from a partial that the caller may
still change. `SpeculationConfig::partial_stable_for` (server configuration
`speculation.partial_stable_ms`, validated not above `max_age`; **default 0 = no gate, the
behaviour is unchanged and speculation stays opt-in**) delays the partial candidate of a VAD
stop. It is offered `partial_stable_for` after the stop (the partial text is the final segments
heard plus the latest interim, normalised; a change of it after the stop restarts the wait), and
only if

- the caller stayed silent: a `VadStart` cancels the wait, so a caller who goes on costs **no
  request at all**, not even a withdrawn one (without the gate the partial is offered at the stop
  and withdrawn at the resumption);
- the text did not change: an interim that changes it restarts the wait (the STT is still
  moving);
- no final came first: the STT final of the stop supersedes the waiting partial. A final is the
  STT's own stable text ("the STT marks it stable"), so final-based candidates are never held.
  No STT adapter exposes any other stability flag today (the transcript frame carries only
  `is_final`), so the time is the only knob.

The adaptive policy ends the turn a few milliseconds after the stop and holds it for the final;
the gated partial is offered then too (`offer_stable_partial`; it was dropped at first because
the turn was no longer `UserPaused`, which only the real-time run showed).

**Resumed speech** (`brisvox-bench speculation`, scenario `resumed after P ms`): every utterance
of the three fixtures is cut in two at the middle of its audio, the halves P ms apart (zeros), the
transcript split after half of the words; the turn is the whole utterance (fixed silence 900 ms,
longer than the pause plus the quiet the cut lands next to), the answers are the same. The VAD
stops 300 ms into the pause and the engine sees the resumption about 40 ms after the speech
restarts, i.e. (P - 260) ms after the stop: 90 / 140 / 340 ms for P = 350 / 400 / 600. Paused
clock, 120 turns per row, LLM first token 350 ms (`bench/results/speculation-fixtures.md` has all
rows, the fast LLM and the counters). Extra LLM requests per turn (`started - committed`):

| pause | converged: no gate / 100 ms / 200 ms | lagging: no gate / 100 ms / 200 ms | final only |
|---|---|---|---|
| 350 ms | 1.00 / 0.50 / **0.04** | 2.04 / 1.48 / **0.32** | 0.04 |
| 400 ms | 1.00 / 1.00 / **0.30** | 2.28 / 2.21 / **0.58** | 0.28 |
| 600 ms | 1.00 / 1.00 / 1.00 | 2.98 / 2.90 / 1.48 | 0.97 |

Withdrawn runs follow the same pattern (400 ms, converged: 120 of 120 turns withdrawn without the
gate, 36 with 200 ms). The latency of the resumed calls does not suffer: V2V p50 is 1402 ms with
speculation off and 1044-1101 ms with it on in every cell (converged 1044 / 1056 / 1057, lagging
1101 / 1079 / 1070 at 400 ms), except lagging interims at 600 ms without a gate or with 100 ms,
where nothing commits and it is +32 ms. The turn ends at the last stop anyway. What is left at 350
and 400 ms is the first half's STT final when it arrived before the resumption (log-normal
around 150 ms): it is a candidate of its own and the gate does not hold it. At 600 ms the
resumption comes after the final in nearly every turn, so the gate saves nothing beyond what
`on_partial = false` already saves (0.97 extra requests per turn is the final's candidate).

What is asserted where: the eval test (`the_gate_saves_the_request_of_a_caller_who_resumes_...`)
asserts that at 350 ms the gated converged mode wastes no more requests and withdrawn runs than
final only (the partial adds none), that the gated lagging mode wastes at most a third of the
ungated one, and at 400 ms that the gate at least halves both. The proof that a caller who
resumes costs **no request of any kind** is the driver test
`a_caller_who_resumes_inside_the_gate_costs_no_candidate_at_all` (no final involved); in the eval
the final-based candidates remain, because they are not gated.

**Continuous speech: what the gate costs** (same runs, scenario `continuous`, V2V p50 delta
against off, mean head start of a committed run in brackets):

| policy | converged: no gate / 100 / 200 ms | lagging: no gate / 100 / 200 ms | final only |
|---|---|---|---|
| adaptive | -141 (154) / -41 (54) / -5 (51, 12% of turns) | +2 / +2 / -5 | +0 |
| fixed 450 ms | -181 (181) / -81 (81) / -45 (40) | -38 / -38 / -45 | -40 |
| fixed 700 ms | -345 (420) / -309 (320) / -279 (273) | -277 / -277 / -279 | -277 |

Extra requests per turn in the lagging column: 1.00 / 1.00 / 0.12 in all three policies. Over the websocket in real time (32 turns per row,
`bench/results/v2v-speculation-gate.md`): adaptive, converged interims -165 ms (no gate), -53 ms
(100 ms), -3 ms (200 ms); fixed 700 ms, converged -306 ms with a 100 ms gate (head start 303
ms), lagging -262 ms and 1.00 extra requests per turn without a gate, -260 ms and 0.00 with a
200 ms gate.

**Reading it.**
- The gate costs what it waits: a converged partial loses `min(gate, STT final latency)` of its
  head start (150 ms median here), so a gate at or above the finalize round trip is final-only:
  the partial is superseded by the final in about 88% of the turns and the rest start a run
  that is still a hit. Nothing else is lost: a lagging partial never gained anything.
- It saves exactly the requests of resumptions that come inside the wait: with the VAD of the
  benchmarks (300 ms stop delay) a 200 ms gate covers pauses up to about 450 ms, a 100 ms gate
  only pauses up to about 350 ms. Longer pauses are answered by the final's candidate, which
  only a different mechanism could hold (see the follow-up).
- On the adaptive policy the whole prize is the finalize round trip, so every millisecond of
  gate is taken from it: it is only worth using where the STT's finalize time is much longer than
  the gate (a slow finalize leaves room for a gate of half of it) or where callers pause
  mid-sentence a lot and the hit rate of the partial is high. Where the finalize is as fast as
  here, the choice is between no gate (a bet on the caller not pausing) and `on_partial = false`
  (no request from a partial at all); a gate of 200 ms is just `on_partial = false` with extra
  steps.
- Defaults stay conservative: gate 0 and speculation off, as recommended above. The
  recommendation of that section is unchanged (fixed silence of 600 ms or more: `on_partial =
  false`; adaptive: `on_partial` once the live hit rate is known); a deployment that wants
  `on_partial` and has callers who pause mid-sentence can set `partial_stable_ms` to about half
  of its STT's measured finalize time.
- Not done, and the next lever if the 600 ms column matters: hold the final-based candidate for
  a short time after the stop too (the STT final is stable text, but a caller who is merely
  breathing resumes after it), and offer the partial when the Smart Turn verdict is already
  "complete".

**Reading the engine stages with speculation on.** A committed speculation requests the LLM
before the turn ends, so `turn_end_to_llm_request` has no sample for those turns and
`turn_end_to_first_audio` is shorter than the provider time it lists; the report says so
under the floors.

## Capacity benchmark, Linux with equal container limits (measured 2026-10-13)

*Measured before the pacer phase was turned off by default: the production pacer still delayed the first frame of every call by its phase (0-19 ms), as in every capacity table of this document dated 2026-10-13 and 2026-10-14. The effect on the maximum is not measurable (A/B in "Pacer phase: Linux measurements", 350 and 300 calls without the phase, 325 and 350 with it); the p95 columns at low load carry about 16-18 ms of it. The Pipecat and LiveKit rows have no such offset.*

**These are the framework-only capacity figures: quote them second, labelled as such.** Linux,
equal limits for both systems, mock providers (no keys, and **no models**: an energy VAD and a
fixed Smart Turn answer, the framework cost only, same caveats as the Windows section below).
The capacity with the real Silero VAD and Smart Turn models is far lower and is the figure to
quote first: "Capacity with the models" below. The Windows numbers in the section after
that stay as the secondary, trend-only data.

**Setup (WSL2 Linux kernel 6.6, Docker, mock providers).** WSL2 Ubuntu on a 32-core Windows 11 PC,
Linux kernel 6.6.87.2-microsoft-standard-WSL2, Docker 29.8.0, release build of `brisvox-bench`
built in `rust:1.99.0-slim-bookworm`, Pipecat 1.12.0 on Python 3.13 (`python:3.13-slim-bookworm`,
the pinned lock). Each server runs in a container with `--cpus=4 --cpuset-cpus=0-3 --memory=4g
--memory-swap=4g --network host --ulimit nofile=1048576:1048576`; the load generator runs on the
host pinned to other cores (`taskset -c 4-11`, 8 tokio workers) and never used more than 1.7
cores; its largest send lateness over all steps was 37 ms (4 ms at the capacity steps) and no
frame went out more than 100 ms late, so no step was generator limited. Commands:
`bench/capacity_linux.sh`, one fresh container per run, 3 runs per configuration,
`bench/capacity_summary.py` for the medians. brisvox is `brisvox-bench engine-server` with 4 tokio
workers (the cgroup quota) and 2 pool threads (half the CPUs, as `PoolConfig::default`), the
production queue of 1024 and the priority line, up to 4096 calls; Pipecat is `bot.py
--mock-providers --workers 4` (4 uvicorn processes, one event loop each). SLO: p95 V2V <= 950 +
250 = 1200 ms, no failed call, no lost turn. brisvox's ramp was 1, 50, 200, 400, 800, 1200, 1600,
2000, 2400, 3000 bisected to 100 calls; Pipecat's 1, 20, 40, 80, 120, ... bisected to 10 calls.

| system (4 CPUs, 4 GiB) | runs: max concurrent calls within the SLO | median | first step that failed | p95 V2V at the max | server CPU at the max | CPU per call | RSS peak at the max |
|---|---|---:|---|---:|---:|---:|---:|
| **brisvox** | 2100, 2100, 2200 | **2100** | 2200 or 2300 (p95 1250-1278 ms) | 1142-1188 ms | 2.9-3.0 cores | 1.4 mcores | 1.84-1.88 GB |
| Pipecat 1.12.0, 4 processes, defaults | 70, 70, 70 | **70** | 80 (p95 1256-1376 ms; once 32 turns talked over) | 1064-1097 ms | 2.86-2.88 cores | 41 mcores | 1.58-1.60 GB |
| Pipecat 1.12.0, 4 processes, `OMP_NUM_THREADS=OPENBLAS_NUM_THREADS=MKL_NUM_THREADS=1` ("tuned") | 70, 90, 90 | **90** | 80 or 100 (p95 2.6 s and 9.4 s, or 163 of 400 turns without an answer) | 1036-1060 ms | 2.0-2.45 cores | 27-29 mcores | 1.57-1.89 GB |

With the same 4-CPU container brisvox carried **23 times** the calls of tuned Pipecat (2100
against 90) and **30 times** those of Pipecat with its defaults (70), on 1.4 against 27-41
millicores per call (19 to 29 times less CPU per call). The one-call V2V is the floor plus 35 ms
for brisvox (p50 985 ms) and plus 58 ms for Pipecat (1008 ms). The ramps of the first runs, at
the same load (p95 V2V of the answered turns in ms, server CPU in cores):

| concurrent calls | brisvox | Pipecat, defaults | Pipecat, tuned |
|---:|---:|---:|---:|
| 1 | 996, 0.02 | 1028, 0.08 | 1027, 0.05 |
| 20 | | 1041, 1.03 | 1039, 0.67 |
| 40 | | 1069, 1.95 | 1105, 1.25 |
| 50 | 1001, 0.14 | | |
| 70 | | 1097, 2.88 | 1036, 2.03 |
| 80 | | **1377, 2.90 (fail)** | **2613, 2.15 (fail)** |
| 200 | 1001, 0.38 | | |
| 800 | 1002, 1.25 | | |
| 1600 | 1019, 2.43 | | |
| 2000 | 1151, 2.94 | | |
| 2100 | 1142, 2.98 | | |
| 2200 | **1278, 3.13 (fail)** | | |

Files: `bench/results/capacity-{brisvox,pipecat,pipecat-tuned}-linux-2026-10-13.{md,json}` (the
summaries: a per-run table and the median), `...-run{1,2,3}.{md,json}` (the ramps with the
environment, the command and every step) and the ablation below.

What the numbers say, and how far to trust them:
- **A ratio of two systems measured by one client, not an absolute capacity.** Both run over
  mock providers with the same fixed latencies, no model inference and no TLS, on a WSL2 virtual
  machine on a shared desktop. A deployment with the Silero and Smart Turn models and real
  providers needs more CPU per call than any row here, and the ranking can move with where a
  system spends its time (neither runs a model here).
- **Repeat spread.** brisvox 2100, 2100, 2200 (bisection granularity 100); Pipecat defaults 70
  three times; tuned 70, 90, 90 (granularity 10). The runs repeat to one step, in contrast to the
  Windows desktop, where Pipecat with four processes moved between 110 and 160.
- **How each fails.** Pipecat at 80 calls fails at 2.9 of its 4 cores: the kernel spreads the
  connections over the four processes without balancing them, so the busiest event loop
  saturates first (not checked per process); p95 climbs to 1.3-2.6 s and turns are talked over
  or lost, in one tuned run p95 was 9.4 s at 100 calls. brisvox's p95 stays at 1.00 s up to
  1200 calls, 1.02 s at 1600 and 1.15 s at 2000, then passes the 1200 ms line between 2100 and
  2200 calls at 3.0-3.1 of its 4 cores (the pool and the CFS quota periods are the likely
  limit; not isolated). At 2400 calls 94 of 9600 turns got no answer and p95 was 1.8 s.
- **Pipecat's thread oversubscription is a configuration effect, reported both ways.** With the
  library defaults numpy's BLAS threads (one pool per process, 4 processes on 4 cores) made
  Pipecat use 41 millicores per call; pinning them to one thread brought that to 27-29 and the
  capacity from 70 to 90 (median). The tuned row is the fair baseline for the claim ("brisvox:
  23 times"); the defaults row is what `python bot.py` does out of the box. A single Pipecat
  process (the default Pipecat setup) and other tunings (a balancer in front, a Smart Turn executor per
  process) were not run on Linux.
- **RSS** is the peak of the process (tree) during the step and carries over from earlier steps
  (neither server returns memory), so it is not a per-call figure. For scale: brisvox 19 MB with
  one call and 1.9 GB at 2100 calls (about 0.9 MB per call), Pipecat 580 MB with one call (four
  processes, a prewarmed pipeline each) and 1.6-1.9 GB at 70-90 calls.
- **Not measured here:** native hardware without the WSL2 layer, the models, real providers,
  other fixtures, interrupting callers, other CPU limits (2 or 8 CPUs), more than one container.

**Three defects of the first Linux attempts, found and fixed in this pass** (they changed the
numbers, so they are recorded):
1. **The file-descriptor limit.** Docker gives a container a soft `RLIMIT_NOFILE` of 1024 and
   the server does not raise it. The first brisvox run (before `--ulimit nofile`) stopped at
   exactly 1017 concurrent calls whatever the CPU left: at 1100 calls 166 calls (83 slots x 2)
   failed with "connect timed out" while `accept()` returned `EMFILE`, at 1200 calls 366
   (183 x 2), identically in three runs. A production container with the default has the same
   cap: docs/DEPLOY.md, "Sizing". The ablation is
   `capacity-brisvox-nofile1024-linux-2026-10-13.{md,json}` (`NOFILE=1024
   bench/capacity_linux.sh`: 1000 pass, 1100 fail). This is a cap by configuration, not by the
   engine, and not part of the table above. Follow-up, done: the server raises its soft
   limit itself (to the hard limit, at startup).
2. **The resource probe double-counted on Linux.** `brisvox-bench capacity --pid` summed the CPU
   time and resident memory of every entry of the process tree, and on Linux `sysinfo` lists each
   thread as a process of its own that reports the whole process's CPU time and RSS. A
   one-process server showed "7 processes", about twice its CPU (6.3 "cores" in a 4-CPU
   container) and an RSS above its memory limit. `capacity/probe.rs` now skips thread entries
   (`thread_kind()`); after the fix the server reads as 1 process and its RSS agrees with the
   cgroup (`memory.peak` 605 MiB against 637 MB read for 800 calls). The pass/fail columns were
   not affected, but the first set of runs was discarded and repeated. Windows has no thread
   entries, so the Windows files are unchanged.
3. **The script itself:** the generator needed `--fixtures-dir` (the default path is the build
   machine's), `--network host` (no userland proxy in the measurement), more generator workers
   and cores, the `nofile` limit, a repeat count and a bench build capped at 4 jobs; fixed in
   `bench/capacity_linux.sh`.

**Reproducing it:** `SYSTEMS=brisvox RUNS=3 STEPS=1,50,200,400,800,1200,1600,2000,2400,3000
RESOLUTION=100 bench/capacity_linux.sh`; `SYSTEMS=pipecat RUNS=3 STEPS=1,20,40,80,120,160,240,320
bench/capacity_linux.sh`; the tuned row adds `NAME_SUFFIX=-tuned SERVER_ENV="-e OMP_NUM_THREADS=1
-e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1"`; then `python3 bench/capacity_summary.py
bench/results DATE`. On a native Linux host the same script applies; expect different absolute
numbers.

## Capacity with the models (measured 2026-10-13)

*Measured before the pacer phase was turned off by default (phase on for brisvox); the 2026-10-14 A/B with the phase off is in "Pacer phase: Linux measurements" below: the same layout reached 350 and 300 calls.*

**Final, quiet host (2026-10-13 23:00; quote these).** Same container and SLO as below,
nothing else running on the machine, brisvox runs interleaved by layout:

| system | runs | median | at the max |
|---|---|---:|---|
| brisvox, Smart Turn executor (default) | 325, 325, 325 | **325** | p95 957-1185 ms, 2.5-2.6 cores, RSS ~425 MB |
| brisvox, shared pool (`--turn-threads 0`) | 300, 275, 300 | 300 | p95 985-1125 ms, 2.3-2.5 cores, RSS ~380 MB |
| Pipecat 1.12.0 with its models, tuned | 20, 15, 20 | **20** | p95 843-960 ms, ~1 core, RSS 3.0-4.3 GB (PSS 3.0-4.2 GB, see below); fails at 20-25 (memory: connection resets at the 4 GiB limit) |

brisvox with the models: about 16 times the calls of Pipecat in about a tenth of the memory. The
busy-host runs below (200 and 250) were depressed by builds sharing the CPUs.

**LiveKit Agents (measured 2026-10-14; `bench/livekit_baseline/README.md`).** LiveKit Agents
1.8.7 (Silero VAD and the local audio turn detector `v1-mini`, the session defaults; a process per
job) over the same mock providers and in the same container (`--cpus=4 --memory=4g`), against a
LiveKit server 1.13.9 in a container of its own (not in the budget: 0.44-0.47 cores, 276-294 MB at
the maximum), with the load generator as 22-25 WebRTC room participants (Python SDK, 8 processes),
the same fixture, ramp and SLO (p95 V2V <= 1200 ms, no failed call, no lost turn), 3 runs:

| LiveKit Agents 1.8.7 | runs | median | at the max |
|---|---|---:|---|
| tuned (turn-detector `unlikely_threshold` 0.05, `load_threshold` 1.0) | 22, 23, 25 | **23** | p95 1171-1192 ms, 2.1-2.2 cores (about 90 mcores per call), 2.9-3.3 GB (PSS) |
| defaults | none | 0 | p95 2.81-2.89 s at one call |

- The defaults fail at one call: the turn detector gives the two fixture utterances an end-of-turn
  probability (0.32 and 0.35) just under its English threshold (0.36) on about 45 % of the turns, so
  the session waits `max_delay` (2.5 s) and V2V is 2.8 s on those turns. The tuned variant lowers the
  threshold so the verdict does not hold the turn; the model still runs on every turn, so its cost
  is in the numbers.
- The one-call V2V is 1130-1140 ms (floor 950 ms): about 100 ms of the extra is the WebRTC input
  path, so 60-70 ms of the 250 ms allowance is left at one call. The maximum is where p95 crosses
  1200 ms or calls get no agent (all failed calls were second-round calls, a burst of job starts and
  exits; probable cause not verified), at 2.1-2.2 of 4 cores and 2.9-3.3 of 4 GiB.
- Memory is counted as PSS: the job processes are forked from a preloaded server and share the
  model pages, a plain sum of `VmRSS` would be 18 GB for 22 calls. CPU is the container's cgroup
  total (finished job processes included).
- Against the rows above: brisvox 200-325, Pipecat (tuned) 20, LiveKit (tuned) 23 calls; about 90
  mcores per call for LiveKit against 8-9 (brisvox) and 70 (Pipecat) with the models. The WebRTC
  path, the SFU and the process per call are the design under comparison; the threats to validity
  (second-round failures, p95 close to the limit, host noise, unchanged UDP buffer) are in the
  README of the baseline.


**Memory counted alike: PSS for all three (measured 2026-10-14).** The memory figures above
were counted differently: brisvox is one process (its RSS), Pipecat's four worker processes were
summed as RSS (shared library and model pages once per process) and LiveKit's job processes as
PSS. `brisvox-bench capacity --pid` now also reads `Pss` from `/proc/<pid>/smaps_rollup` for every
process of the tree (root is needed to read another user's processes; `capacity_linux.sh` stops
otherwise) and records it next to the RSS sum: `pss_peak_bytes` and `pss_before_bytes` per step in
the JSON, a "target PSS before / peak (MB)" column in the markdown (parser tested in
`capacity/probe.rs`). Pipecat's tuned ramp with the models was run again, 3 runs, `MODELS=1
SYSTEMS=pipecat NAME_SUFFIX=-q-tuned`, the steps (1, 5, 10, 15, 20, 25, 30, 40) and resolution (5),
container, mocks and environment variables of 2026-10-13, each run after a quiet-host gate (no
cargo/rustc/link.exe on Windows, WSL 1-minute load below 1.0; the gate result is in each run's
note): `capacity-pipecat-q-tuned-models-linux-2026-10-14*`, a brisvox cross-check in
`capacity-brisvox-q-pss-crosscheck-models-linux-2026-10-14*` (not part of the chart series).

| | max calls (runs) | RSS sum at the max | PSS at the max | PSS per call (baseline included) |
|---|---|---:|---:|---:|
| Pipecat 1.12.0, tuned, 4 workers (6 processes incl. launcher/parent) | 20, 20, 15 (median 20; 2026-10-13: 20, 15, 20) | 4344, 4136, 3230 MB | 4190, 3981, 3073 MB | 210, 199, 205 MB |
| LiveKit Agents 1.8.7, tuned (2026-10-14 runs, 47-53 processes) | 22, 23, 25 | 18012, 18692, 20232 MB | 2935, 3023, 3269 MB | about 130 MB |
| brisvox cross-check (1 process, default layout, one run) | 350 | 432 MB | 429 MB | 1.2 MB |

- Pipecat: the RSS sum overstates the PSS by about 150 MB at every step (3.5-4 % at the max; one
  call 887 vs 736 MB, 20 calls 4344 vs 4190 MB in run 1), so the old comparison was fair to about
  4 % for Pipecat; the large gap was LiveKit's (a plain sum 6 times its PSS). The Pipecat gap does
  not grow with the calls, so what its four workers share is a fixed ~150 MB (not investigated
  further).
- brisvox: one process shares nothing, RSS and PSS differ by 3 MB (0.7 %) in the cross-check at
  350 calls. The cross-check found 350 where the 2026-10-13 runs gave 325 (one run of a later build,
  the earlier runs failed at 350 and were bisected to 325, this one passed 350 and failed at 375); the quoted 325
  and ~425 MB stay. The memory chart uses PSS for Pipecat and LiveKit and the single process' RSS
  (= PSS within 1 %) for brisvox.
- Pipecat's capacity run for this section reproduces the 2026-10-13 result: the same multiset of
  runs (20, 20, 15 vs 20, 15, 20), median 20; the capacity chart now reads the 2026-10-14 files
  (the method is identical; the only change is the extra PSS reading by the sampler on the host,
  outside the container's CPU limit). Runs 1 and 2 failed at 25 calls (8 and 16 of 50 calls failed
  with connection resets; the PSS at that step was 4.3 GB against the container's 4.29 GB limit,
  as in the 2026-10-13 runs), run 3 failed at 20 (5 of 40 calls; PSS 4.3 GB).
- A first launch of the series as a normal user read no PSS (the `-` columns); it was stopped and
  repeated as root. Those partial files were not kept.

**Linux, equal containers (2026-10-13, the figures to quote).** `MODELS=1 bench/capacity_linux.sh`,
WSL2 kernel 6.6, Docker 29, `--cpus=4 --cpuset-cpus=0-3 --memory=4g`, mock providers, both
systems with their real models (brisvox: Silero on tract + Smart Turn v3.2 on ORT, 4 tokio workers,
2 pool threads; Pipecat 1.12.0: its Silero VAD + `LocalSmartTurnAnalyzerV3`, 4 processes,
`OMP/OPENBLAS/MKL_NUM_THREADS=1`), SLO p95 V2V <= 1200 ms, 3 runs each:

| system | runs (max calls within the SLO) | median | at the max | per call |
|---|---|---:|---|---|
| brisvox | 250, 200, 125 | **200** | p95 831-915 ms, 1.6-2.0 cores, RSS 300-390 MB | ~8-9 mcores, ~1.5 MB |
| Pipecat (tuned) | 20, 20, 5 | **20** | p95 810-912 ms, 1.2-1.4 cores, RSS 3.6 GB | ~70 mcores, ~180 MB |

- brisvox fails before its CPU is used up (2.0-2.4 of 4 cores at the failing steps): the 2-thread
  inference pool saturates (Smart Turn ~25 ms per analysis on ORT with 2 intra-op threads) ->
  the Smart Turn threading section below.
- Pipecat is memory-bound: 110-140 MB per call with its models; at 25-30 calls the container
  reaches 4 GiB, workers are killed and restarted (connection resets, CPU reading drops to ~0;
  4 restarts in runs 1 and 2).
- The low runs (brisvox run 3: 125, Pipecat run 3: 5) coincided with heavy cargo builds on the
  shared Windows host (WSL2 shares its CPUs); re-measure on a quiet or native Linux host before
  quoting single numbers. Files: `bench/results/capacity-{brisvox,pipecat-tuned}-models-linux-2026-10-13*`.

The Windows numbers below are the first, provisional measurement.

**Measured so far: Windows only, provisional.** The Linux run could not be started
from the session that wrote it (the sandbox refused to run `wsl`/`docker`; see
"Reproducing" below), so the figures here are the Windows x86_64 development machine (32
logical cores), a release build, the engine server with 4 tokio workers and 2 pool threads like
the 4-CPU container, **but without a CPU limit** (the server may use any core; it used at most
3.1 of them). They show the shape (the cost per call and what saturates) and not the Linux
capacity; `docs/LATENCY.md`'s rule that comparisons come from Linux stands.

| run (3 repeats, ramp 1, 25, 50, 100, 200, 300, ... bisected to 25) | max calls within the SLO | first failing step | p95 V2V at the max | server CPU at the max | RSS peak at the max |
|---|---:|---|---:|---:|---:|
| 2 pool threads (the 4-CPU default) | **200, 200, 200** | 225 (p95 1.46-1.80 s) | 1009, 885, 885 ms | 2.40, 2.26, 1.99 cores | 280 MB |
| 4 pool threads (one run, ablation) | 175 | 200 (p95 1.76 s) | 889 ms | 3.21 cores | 348 MB |

Files: `bench/results/capacity-brisvox-models-windows-2026-10-13-run{1,2,3}.{md,json}`,
`capacity-brisvox-models-pool4-windows-2026-10-13.{md,json}`, and `*.server.log` next to each
(the pool's per-period stats, the Smart Turn / VAD split and the run's totals).

What the numbers say:
- **About 12 millicores per call with the models** (1.18 cores at 100 calls, 2.40 at 200),
  against the 1.4 millicores of the no-model Linux run: the two models are most of a call's CPU.
  By the pool's own accounting at 200 calls: ~50 pool jobs per second per call (the VAD windows,
  batched per 20 ms tick) at ~105 microseconds each, and about one Smart Turn analysis per call
  per 9 s (17-23 analyses/s at 200 calls) at 24-31 ms each of wall time on the pool thread. So
  the pool (2 threads) was busy 44-52% at 200 calls with Smart Turn about 55-60% of its run
  time and the VAD windows and feature extraction the rest; the failing steps were pool
  saturation (busy 99%, waits of 17 ms per job in a failing step), not the websocket, the
  transport or the tokio workers.
- **Smart Turn is the larger model cost per call, the VAD the larger count.** One Silero window
  costs ~0.1 ms, 50 times a second per call, about 0.5% of a core per call; one Smart Turn
  analysis is 25 ms (more than 200 windows) and runs about 0.11 times a second per call, the
  same order, and it also holds a pool thread for its whole duration, which is what delays other
  calls' VAD windows when analyses bunch up.
- **More pool threads did not help here** (4 threads: 175 calls, and more CPU per call, 18
  against 12 millicores). The likely cause is ONNX Runtime's intra-op worker threads, which spin
  while waiting for work (2 per Smart Turn session, 4 sessions) and burn CPU the stub never
  used; disabling spinning (`session.intra_op.allow_spinning=0`) was the first thing to try; a later A/B tried
  it (section below): it saves CPU but lowers the Windows capacity, so it is not the cause.
- **Expected on the 4-CPU container, not measured:** 4 cores at 12 millicores per call is about
  330 calls by CPU alone and the 2-thread pool saturated at 200-225 on this machine, so the
  honest order of magnitude is a few hundred calls, not 2100. The Linux run since gave a median
  of 200 (Linux table at the top of this section).

**Pipecat with its models.** `bot.py --mock-providers --mock-models real` runs Pipecat's Silero
VAD (brisvox's pinned `silero_vad.onnx` when it is in `BRISVOX_MODELS_DIR`) and its
`LocalSmartTurnAnalyzerV3` (the CPU model bundled with Pipecat, `--smart-turn-threads 1`), over
the same mock providers. `MODELS=1 SYSTEMS=pipecat NAME_SUFFIX=-tuned SERVER_ENV="-e
OMP_NUM_THREADS=1 -e OPENBLAS_NUM_THREADS=1 -e MKL_NUM_THREADS=1" bench/capacity_linux.sh` runs
it in the equal 4-CPU container. Measured since on Linux: median 20 calls, memory-bound (Linux table at the top of this section).

**Reproducing it (Linux).** `MODELS=1 RUNS=3 SYSTEMS=brisvox STEPS=1,25,50,100,200,300,400,600,800,1000,1400
RESOLUTION=25 bench/capacity_linux.sh` (builds `--features models` in `rust:1.99.0-slim-trixie`:
the prebuilt static ONNX Runtime needs glibc >= 2.38; mounts `models/` read-only; leaves
`capacity-brisvox-models-linux-<date>-run<N>.{md,json,server.log}`), then the same with
`SYSTEMS=pipecat NAME_SUFFIX=-tuned ... STEPS=1,5,10,20,30,40,50,60,70,80,100 RESOLUTION=5`,
then `python3 bench/capacity_summary.py bench/results <date>`. Windows: `brisvox-bench capacity
--sim --models models --pool-threads 2 --server-workers 4 --stats-every-ms 10000 ...` with a
build of `cargo build --release -p brisvox-bench --features models`.

### Unloaded floor: brisvox vs Pipecat with the same mocks (measured 2026-10-14)

Question: with the same mock providers (STT final 150 ms after the VAD stop, LLM first token 350 ms,
TTS first byte 150 ms) the one-call step of the Linux capacity runs with the models shows Pipecat
1.12 at p50 787 ms and brisvox at 805 ms (`capacity-pipecat-q-tuned-models-linux-2026-10-13-run1.json`,
`capacity-brisvox-q-turnexec-r1-models-linux-2026-10-13-run1.json`, `steps[0].v2v`). Where do the
18 ms go?

**Answer.** Two effects of a few tens of milliseconds that nearly cancel, neither of them a stage
of the engine: (1) the VAD stop is **40 ms later** in brisvox on the second turn of the fixture
(20 ms on the first): the same nominal 100 ms hangover is **4 Silero windows (128 ms)** in brisvox
and **3 windows (96 ms)** in Pipecat; (2) Pipecat's output path is **21 ms slower** than brisvox's
(its streaming resampler gives no audio for the first 40 ms TTS chunk, so the first frame waits for
the second chunk). Net: brisvox +40 - 21 = **+18 ms** on the second turn. The capacity step has
2 calls x 2 turns, and the nearest-rank p50 of 4 samples is the second smallest, which is the
second turn of each call, so its 805.3 vs 787.1 ms is exactly this row.

**Method.** WSL2 (kernel 6.6), Docker, the containers of the capacity runs (`--cpus=4
--cpuset-cpus=0-3`, `--network host`, the real models, Silero and Smart Turn v3.2, on both sides),
the `en_two_utterances` fixture, one call at a time, 15 calls (30 turns) per configuration, the
servers up together and the calls interleaved, the client (`brisvox-bench v2v --url`) pinned to
other cores. brisvox's stage times are its own turn timeline (`BRISVOX_BENCH_TURN_LOG=1` on
`brisvox-bench engine-server` prints one `turn-log` line per turn: every timeline event in ms
relative to the end of speech); Pipecat's come from `bench/pipecat_baseline/stage_probe.py` (an
observer on the real pipeline, the same `bot.py`). The VAD stop in audio time is the number of
input frames seen when Pipecat reported the stop (logged), and for brisvox the fixture's Silero
probabilities run through its stop rule (checked against the live runs below).

Means over 15 calls, **second turn** (the fixture's end of speech at 13.43 s of the stream):

| stage | brisvox | Pipecat | brisvox - Pipecat |
|---|---:|---:|---:|
| end of speech -> VAD stop decision (audio time, 20 ms frames) | 150 ms | 110 ms | **+40** |
| VAD stop -> STT final (mock 150 ms + timer) | 151.4 | 151.2 | +0.2 |
| STT final -> end of turn (the Smart Turn verdict is already in) | 0.0 | 0.2 | -0.2 |
| end of turn -> LLM first token (mock 350 ms) | 351.4 | 351.0 | +0.4 |
| LLM first token -> TTS first audio (mock 150 ms) | 151.1 | 151.1 | 0.0 |
| TTS first audio -> first frame handed to the websocket | 0.1 | 21.1 | **-21.0** |
| websocket -> client arrival (V2V minus the rows above) | 1.8 | 3.0 | -1.2 |
| **V2V at the client** | **805.8** | **787.6** | **+18.2** |

First turn of each call (end of speech at 6.67 s): brisvox 818.1, Pipecat 808.3 (+9.8). The VAD
row is +20 ms (150 vs 130), the output row -21 ms, and brisvox pays its **pacer phase** (below), a
per-call offset of 0-19 ms on the first bot frame of the call (mean 10.9 ms over these 15 calls).
Over both turns brisvox is 811.9 and Pipecat 798.0 (+13.9).

**Why 4 windows against 3.** Silero runs on 512-sample windows (32 ms) in both. brisvox's
`VadDetector` confirms the stop at the first silent window whose end is at least `min_silence`
(100 ms) after the end of the last speech window: 3 windows are 96 ms, not enough, so 4 windows =
128 ms. Pipecat counts `round(stop_secs / 0.032)` = `round(3.125)` = 3 windows = 96 ms. The same
model file, the same threshold (0.5), the same 20 ms input frames. The fixture's speech probability
falls from 0.99 to 0.003 within one window, so the difference is exactly one window, which the
20 ms frame grid turns into 20 ms on the first utterance and 40 ms on the second (32 ms on average
over arbitrary audio). Pipecat's volume gate (`min_volume` 0.6) changes nothing on this fixture (the
same stops with it off). Confirmed in both directions on Linux, 15 calls each:

| configuration | first turn | second turn |
|---|---:|---:|
| Pipecat `stop_secs` 0.1 (as mapped: 3 windows) | 808.3 | 787.6 |
| Pipecat `--vad-stop-ms 128` (4 windows) | 828.8 (+20.5) | 828.5 (+40.9) |
| brisvox `min_silence` 100 ms (the default: 4 windows) | 818.1 | 805.8 |
| brisvox `min_silence` 90 ms (3 windows) | 797.9 (-19.4) | 765.7 (-40.0) |

(The 90 ms row is compared with 817.3 / 805.7, the 100 ms row measured beside it.) With the same
effective hangover brisvox is the faster system either way: 781.8 against 798.0 ms over both turns
at 3 windows, 811.9 against 828.7 at 4 windows (16 to 17 ms on average, 22 ms on the second turn
at 3 windows).

**Why Pipecat's output path is 21 ms slower.** `stage_probe.py` shows the first TTS audio frame
reaching the transport and the websocket write 21.1 ms later (20.5-22.2 over 30 turns). The mock TTS
sends 40 ms chunks 20 ms apart; Pipecat 1.12's `create_stream_resampler` (soxr, 24 kHz to the
call's 16 kHz) returns 0 samples for the first 40 ms chunk and 1100 samples (69 ms) after the
second, so the first frame leaves one chunk interval (+ 1 ms) later. At 8 kHz it takes four chunks
(measured: 0, 0, 0, 1100 samples). brisvox hands the first 20 ms frame of the first chunk on in
at most 0.1 ms. This is a property of Pipecat's resampler on this chunk shape (a provider with
other chunk sizes changes it); the mocks' chunk shape is the same on both sides by design.

**Where brisvox's own overhead is.** After the VAD stop brisvox spends at most 0.1 ms in the
engine's stages and 1.8 ms from the websocket write to the client (second turn), on top of the
mocks' nominal 150 + 350 + 150 = 650 ms and three timer overshoots of about 1 ms each. The Smart
Turn verdict is in before the STT final in both systems (brisvox: 0.0 ms between the final and the
end of turn; Pipecat's analysis takes 26-52 ms and its stop strategy awaits it inline, still
behind the 150 ms final), so the turn-decision wait is 0 on both sides in this setup (with a
real STT whose final comes sooner than the model it would not be).

**The pacer phase (first turn only; removed from the default, see below).** `OutputPacer`
released the first frame of a *session* a per-session offset after its arrival, `phase_offset(seed)`
in [0, 20 ms) (spreads the timer wake-ups of co-started sessions); the transport session seeds
it with the session id (the call number in the bench). The first-turn V2V of a call followed it one to
one: over the 15 calls V2V = 807.4 ms + 0.99 x phase (r = 0.98). It cost the first answer of each call
9.5 ms on average (0 to 19) and nothing afterwards. Pipecat has no such offset.

**What was not changed, and the options.** None of this is a defect in the engine's stages, and
each lever is a trade-off, so no default moved:

1. *VAD hangover in windows.* `VadParams::default().min_silence` of 90 ms (3 windows; or a rule
   that rounds to the nearest window like Pipecat) lowers V2V by 20 to 40 ms (30 on average over the two
   utterances, deterministic: the 90 ms row). The cost: a pause of 96-128 ms inside speech then ends the VAD
   segment, which sends a `Finalize` to the STT, runs a Smart Turn analysis and can end the turn
   early when the model says "complete" (the turn machine already takes a resumption back). On the
   7 fixtures (17 read utterances, `fixtures/` and `fixtures/large/`) no pause fell in that band
   (a 64 ms hangover makes one extra stop; 96, 100, 128 and 160 ms none), but conversational
   speech has more such pauses than read speech. Use 90, not 96: the detector compares wall-clock
   window stamps, so 3 windows measure 95.9-96.1 ms and a 96 ms limit flips at random (measured: 11
   of 15 second turns stayed at 4 windows). `interruption.vad_hangover_ms` (100) and the early
   analysis' `fall_delay` follow `min_silence`.
2. *Pacer phase.* Done (below): the first frame leaves at once; the phase is an opt-in.
3. *The comparison.* The mapping `stop_secs = 0.1` is nominally equal and effectively 3 windows
   against 4. For a table that names an equal effective hangover run `bot.py --vad-stop-ms 128` or
   brisvox at 90 ms. The 21 ms of Pipecat's resampler is part of Pipecat as shipped and stays in
   the comparison.

**The first frame of an answer leaves at once (2026-10-14).** The pacing clock now starts at
the first audio frame after an idle period (session start, an interrupt, a finished turn), so the
first frame of a call's first answer leaves the instant the engine hands it over, with no timer, and
the 20 ms grid runs from there. The per-session phase is kept as an opt-in
(`OutputPacer::with_first_frame_phase`, `SessionConfig::first_frame_phase`, both off by default): it
bought spreading the timer wake-ups of sessions started in the same millisecond, which only
matters for a synchronised start (a greeting at call start, a load generator); every later turn
already started at once, and the grid of each turn is anchored at its own first frame, which in
real traffic arrives at a random time. Considered and not done: keeping the phase as a lead on the
grid (first frame at once, the following ones up to 19 ms early). It would keep the spreading, but a
source delivering in real time would then be "late" by the lead on every frame (a phantom lag in
`PacerStats::worst_lag`) and the position the transport reports (audio released) would run up to a
frame ahead of the clock. Unit tests (paused clock, `crates/brisvox-audio/tests/pacer.rs`):
the first frame of 256 seeds leaves at its arrival with no timer, a first answer after 2 s of idle
is not delayed, an interrupt flushes and the next turn starts at once; the earlier pacer tests run with the
phase on. No per-frame cost: one bool read on the first frame, no allocation.

Re-measured on Windows (release build, `v2v --sim`, `en_two_utterances`, 2 x 15 calls plus the
memory calls per configuration, 76 first and 76 second turns each, the two configurations
interleaved on one binary; `BRISVOX_BENCH_PACER_PHASE=1` is the bench-only switch for "before"):

| turn | before (phase on) mean / p50 | after (default) mean / p50 | change of the mean |
|---|---:|---:|---:|
| first turn of a call | 1015.8 / 1011.9 ms | 1001.6 / 1003.5 ms | -14.2 ms |
| second turn (control, no phase before or after) | 1001.0 / 999.9 ms | 1002.2 / 1002.1 ms | +1.2 ms |

The expected change is the mean phase, 9.5 ms; the measured -14 ms is inside the noise of this
machine (standard deviation 11-14 ms per turn, 15.6 ms timer tick, other work running), so read it as
"the first turn no longer sits above the second" (it was +14.8 ms above, now -0.6 ms), not as a
precise number. The Linux figure is the one to quote: the first-turn offset found earlier (0-19 ms, mean 9.5 ms) is gone
by construction (the unit test shows the first frame at its arrival), and it is measured below.

**Pacer phase: Linux measurements (2026-10-14).** Both tests ran on the quiet host (the gate of
`bench/capacity_linux.sh` before every run: no `cargo`/`rustc`/`link.exe` on Windows, WSL 1-minute
load below 1.0; the load each run saw is in its note), in the containers of the capacity runs
(`--cpus=4 --cpuset-cpus=0-3 --memory=4g --network host`, the real models, `brisvox-bench
engine-server`), the client pinned to cores 4-11. The switch is the bench-only
`BRISVOX_BENCH_PACER_PHASE=1` (read by the engine server for every call; `docker run -e`), the same
binary and the same host for both sides.

*(a) First turn, one call at a time.* The first-turn harness (`brisvox-bench v2v --url`, fixture
`en_two_utterances`, `--pause-ms 200`): two servers up together (phase off, phase on), one warm-up
call each (discarded), then 15 calls each, one at a time and interleaved (off/on and on/off
alternating), gate: load1 0.31.

| V2V at the client | phase off (default) | phase on (before the change) | on - off |
|---|---:|---:|---:|
| first turn of a call: mean / p50 | **805.7** / 805.8 ms | 817.0 / 816.3 ms | **+11.2 ms** (standard error 1.4) |
| first turn: sd, min .. max | 0.8, 804.7 .. 807.7 | 5.4, 808.4 .. 825.0 | |
| second turn (control: no phase either way) mean | 805.5 ms | 805.8 ms | +0.3 ms |

With the phase off the first turn sits on the second turn (805.7 against 805.5 ms) and has the
spread of any turn (sd 0.8 ms); with the phase on every call's first turn is 2.7 to 19.3 ms above
that. This confirms the earlier investigation (a mean offset of 9.5 ms, 0.99 ms of V2V per ms of phase): **the change
is 11 ms (this sample's phases averaged somewhat above 9.5) on the first answer of every call on
Linux**, 805.7 ms against 817.0 ms.

*(b) Capacity A/B with the models, phase on against off.* `MODELS=1 SYSTEMS=brisvox RUNS=1
QUIET_GATE=1`, steps 1, 50, 100, 200, 250, 300, 350, 400, 500, bisected at 25, SLO p95 V2V <= 1200
ms and no failed call; run as root for PSS. Four runs in two pairs, the second pair in the reverse
order (to expose a first-run or thermal bias): A = off then on, B = on then off. The question: does
the synchronised start of many sessions in the same millisecond (no phase) cost anything under load?

| run | phase | max calls | p50 / p95 / p99 at 250 calls | at 300 calls (p50 / p95) | at the max: p95, cores | first failing step (p95) |
|---|---|---:|---|---|---|---|
| A1 | off | **350** | 805 / 807 / 808 ms | 808 / 818 ms | 925 ms, 2.50 | 375 (1317 ms) |
| A2 | on | 325 | 806 / 824 / 826 ms | 812 / 833 ms | 904 ms, 2.39 | 350 (1416 ms) |
| B1 | on | 350 | 806 / 824 / 826 ms | 811 / 833 ms | 922 ms, 2.48 | 375 (1551 ms) |
| B2 | off | 300 | 805 / 807 / 807 ms | 811 / 965 ms | 965 ms, 2.35 | 325 (1465 ms) |

- **Not measurable.** Phase off: 350 and 300 calls (mean 325); phase on: 325 and 350 (mean 337.5).
  The runs differ by one or two bisection steps (25 calls) in both directions, which is the
  run-to-run spread of this host (the same layout gave 325, 325, 325 on 2026-10-13 and 350 in the
  PSS cross-check, failing at 375). The knee is the pool (CPU 2.4-2.6 of 4 cores at the failing
  steps), not the pacer's wake-ups.
- Below the knee nothing separates the two except the phase's own cost: p50 805-808 ms both ways, p95
  and p99 about 16-18 ms higher with the phase on (806-809 ms off, 816-825 ms on at steps 1-250),
  which is the 0-19 ms offset of the first answer of each call showing in the tail (and the (a)
  result under load).
- No late frame at any step of any run (0 of 0.4-0.6 million timed frames at the larger steps; the
  client's largest send lateness 2-19 ms, a harness figure).
- Wake p99 is not available: the pacer's `worst_lag` is a per-session counter that the bench server
  does not export. The pool's per-window VAD wait (the nearest figure) is in the `*.server.log`
  files.
- The auditor's mitigation (first frame at once, a session-seeded offset applied from the second
  frame on) was therefore **not implemented**: there is no cost to remove, and it would add a
  tuned offset to every call's second frame. A synchronised start of very many sessions (a greeting
  at call start from a load generator) is the case where `first_frame_phase` stays the switch; the
  capacity ramp starts its calls over `spread_ms` = 4000 ms, so it does not start them in the same
  millisecond, and a test that does is not part of this measurement.

Files: `bench/results/capacity-brisvox-phase-{off,on}-models-linux-2026-10-14-run1.{md,json,server.log}`
(A1, A2) and `capacity-brisvox-phase-{on,off}-b-models-linux-2026-10-14-run1.*` (B1, B2); the
first-turn reports are in `bench/results/v2v-pacer-phase-linux-2026-10-14/`. Reproduce the capacity
runs (root, WSL; `NAME_SUFFIX` is `-phase-off` / `-phase-on`):

```sh
MODELS=1 SYSTEMS=brisvox RUNS=1 QUIET_GATE=1 RESOLUTION=25 STEPS=1,50,100,200,250,300,350,400,500 \
  NAME_SUFFIX=-phase-on SERVER_ENV="-e BRISVOX_BENCH_PACER_PHASE=1" bench/capacity_linux.sh
```

**Windows.** The same test on Windows (15.6 ms timer tick, 70 turns per system, with the models):
both systems at 838.5 ms. The tick rounds Pipecat's 20 ms resampler wait up to 31 ms and bunches
the client's frames, so the 20/40 ms VAD steps do not show (brisvox at `min_silence` 96 ms moved
only -6 ms there; at 32 ms -55 ms). Do not compare differences under about 30 ms on Windows.

Reproduce (Linux; `BRISVOX_BENCH_VAD_MIN_SILENCE_MS=90` on the server is the bench-only override
of the detector's `min_silence` for the 3-window row):

```sh
BRISVOX_BENCH_TURN_LOG=1 brisvox-bench engine-server --bind 127.0.0.1:8100 --models models --max-calls 8 &
brisvox-bench v2v --url ws://127.0.0.1:8100/call --fixtures en_two_utterances --runs 5 --pause-ms 200
PCTRACE_FILE=/tmp/pc.txt python bench/pipecat_baseline/stage_probe.py --mock-providers --mock-models real --port 8102 [--vad-stop-ms 128]
```

## Model CPU per call: Smart Turn threading and the Silero backend (measured 2026-10-13)

**Result in one line: the defaults did not change.** Turning ONNX Runtime's intra-op spinning off
cuts the CPU a Smart Turn analysis costs in a quiet process by 40% and changes no bit of the
output, but under the capacity load each analysis occupies its pool thread 12-25% longer and the
2-thread pool saturates at 250 calls where it does not with spinning: the pool, not the CPU, is the
limit. Smart Turn stays at 2 intra-op threads with ONNX Runtime's default (spinning on). The knobs
now exist (`OrtOptions`, `SmartTurnOrt::load_with_options`) and the Linux run decides.

**Tool.** `brisvox-bench model-cpu --models DIR` (build with `--features models`;
`crates/brisvox-bench/src/model_cpu.rs`): the process's CPU time (every thread, so ONNX Runtime's
workers and their spinning are in it; sysinfo, accounted per scheduler tick, so every CPU figure is
a mean over batches of 20 analyses and the p50/p99 columns are over batch means) next to the wall
time of each call, configurations interleaved round by round so a noisy neighbour (a Linux
benchmark was running in WSL on this machine) hits all alike. `tight` = analyses back to back,
`paced` = a sleep after each, with the CPU read after the sleep, so what idle workers burn is
counted; a control (the same period with nothing to do) is subtracted from every paced batch.
Windows x86_64 (32 logical cores), release, onnxruntime 1.28 via `ort` 2.0.0-rc.13, a golden
case's 8 s window as input, 30 rounds x 20 analyses per cell. Files:
`bench/results/model-cpu-windows-2026-10-13-run1.md` (40 ms sleep) and `-run2-gap100.md` (100 ms).

Smart Turn, one analysis (run 1; ms):

| intra-op threads | spinning | CPU tight | CPU paced (40 ms sleep) | wall p50 / p99 (tight) | wall p50 (paced) |
|---:|:---:|---:|---:|---:|---:|
| 1 | on / off (no workers) | 34.3 / 34.5 | 35.1 / 34.2 | 33.1 / 46.6 | 33.6 / 33.5 |
| **2 (the default)** | **on** | 41.3 | **72.3** | 19.3 / 25.6 | 20.3 |
| 2 | off | 40.5 | 43.4 | 20.7 / 26.6 | 22.2 |
| 3 | off | 46.9 | 49.1 | 17.7 / 39.3 | 18.8 |
| 4 | off | 52.7 | 53.9 | 16.0 / 28.4 | 16.9 |

Run 2 (quiet machine, 100 ms sleep): 2 threads with spinning 71.1 ms CPU paced (39.5 tight),
spinning off 40.6 (38.7); 1 thread 32.3 (32.5); wall p50 paced 20.0 / 21.1 / 32.2 ms. So the spin
burn is about 30 ms per analysis, the same at 40 and 100 ms between analyses (it is bounded per
analysis, not per second), as much as the analysis itself. 1 thread is 20% cheaper than 2 with
spinning off but 50% slower (33 against 22 ms), which breaks the ~10% latency bound; 3 and 4
threads cost 13% and 24% more CPU for 15% and 24% less latency.

**Exactness.** All 6 configurations ran the 12 golden cases: the probabilities are
**bit-identical** across intra-op 1, 2, 3, 4 and spinning on/off (0 of 12 differ in bits from the
first configuration; worst error against the reference 5.96e-8, the golden rounding; 0 verdict
differences). It is also a test: `probabilities_are_bit_identical_across_session_configurations`
(`crates/brisvox-inference/tests/smart_turn.rs`) compares bits across threads 1-4 x spinning on/off
and against the 1-thread default the goldens were generated with, through warmed sessions.

**Silero, one 32 ms window (CPU per window, mean over batches of 2000; us):**

| backend | run 1 | run 2 | wall p50 / p99 |
|---|---:|---:|---:|
| tract (the server's default) | 61 | 59 | 53 / 85 |
| ONNX Runtime (1 intra-op thread, feature `silero`) | 106 | 100 | 94 / 172 |

tract is 40% cheaper per window here, so the default stays (the comparison needs `--features
models`, which now also builds the ONNX Runtime Silero for it). The two backends agree: max
|p_tract - p_ort| = 1.2e-6 over 872 fixture windows, 0 windows where the 0.5 decision differs. (The
105 us of the earlier model run and the 50-60 us here are the same model under different load and boost states; the
ratio is what carries.)

**Capacity at the old maximum, spinning on (the default; `BRISVOX_BENCH_SMART_TURN=2:1`) against off
(`2:0`), 2 runs each, interleaved on/off/on/off** (`capacity --sim --models models --pool-threads 2
--server-workers 4 --steps 150,200,250 --rounds 2`;
`bench/results/capacity-t118-spin-{on,off}-run{1,2}-windows-2026-10-13.*`; the variable is read by
`RealModels::load`; Windows, no CPU limit, provisional). Per cell: run 1, run 2.

| calls | CPU per call on / off (millicores) | p95 V2V on (ms) | p95 V2V off (ms) | Smart Turn ms per analysis on | off | pool wait per job on | off |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 150 | 12.0, 11.5 / 9.8, 9.1 | 844, 844 | 845, 846 | 21.1, 19.7 | 23.6, 22.2 | 1.2, 1.0 ms | 1.2, 1.2 ms |
| 200 | 10.9, 11.7 / 10.0, 10.0 | 847, 866 | 856, 864 | 20.4, 22.7 | 25.1, 25.0 | 2.3, 3.2 ms | 3.4, 3.8 ms |
| 250 | 10.9, 10.7 / 10.3, 11.2 | 989, 1049 | **1966, 1751** | 23.3, 23.7 | 27.7, 25.8 | 5.4, 5.6 ms | 8.5, 7.3 ms |

Spinning off does save 12-19% of the process CPU per call at 150-200 calls (the quiet-machine
saving survives, diluted by the VAD windows and the rest), but a Smart Turn analysis occupies its
pool thread 12-25% longer (the intra-op worker is woken by the OS for each parallel region instead
of already running), the VAD windows wait longer behind it, and at 250 calls both off runs failed
the SLO (p95 1.75 and 1.97 s) where both on runs passed (0.99 and 1.05 s; limit 1.2 s). The earlier
table failed at 225 with spinning on and 250 passes here (the machine was quieter), so only the
on/off order inside this interleaved set is evidence, not the absolute knee. The earlier hypothesis
"spinning is why more pool threads did not help" is therefore not supported as a way to more
capacity: spinning costs CPU, but it buys pool occupancy, and the pool is the bottleneck on a
machine with spare cores. In a container where CPU is the hard limit it may go the other way.

**Linux confirmation to run** (`bench/capacity_linux.sh` passes `SERVER_ENV` into the container):
`MODELS=1 ... SERVER_ENV="-e BRISVOX_BENCH_SMART_TURN=2:0" NAME_SUFFIX=-spinoff` against the plain
run, interleaved, same steps. Adopt spinning off (flip `DEFAULT_INTRA_OP_SPINNING`) only if the
4-CPU container's capacity and CPU per call improve there. Not done: a server env knob for the
model threads (the server has none today; `OrtOptions` is the hook if the Linux numbers want one),
and a single-thread session on a bigger pool (an analysis 50% longer needs more pool threads).

## Smart Turn off the VAD's critical path (measured 2026-10-13, Windows)

**Result in one line: Smart Turn analyses now run on an executor of their own, and the capacity
with the real models on the 4-CPU-sized server (4 tokio workers, 2 pool threads) goes from
200-250 calls to 300 within the SLO, with nothing changed at low load.** The cause
was structural: an analysis is 20-25 ms on an inference-pool thread, a Silero window is 60 us, and
every analysis made the windows of other calls wait behind it (jobs are not preempted; the
priority lines only order the queue, not a running job). A window that waits stretches
the VAD stop, the Smart Turn start and with them the turn.

**Design.** `brisvox_engine::offload::start_smart_turn_executor(threads, clock)` starts a second
`InferencePool` (threads `brisvox-turn-<i>`, queue 32 jobs per thread, reject newest) that runs
only the analyses and the model's warm-up (`SessionBuilder::with_smart_turn_offload`,
`EngineRunner::with_smart_turn_offload`). Overflow policy: a refused or timed-out analysis is a
failed analysis (`smart turn analysis failed`, counted) and the turn ends at its fallback
deadline, the degradation the turn driver already uses for pool trouble; the VAD's pool is never
involved, so a flood of analyses can no longer lose a VAD stream. Epochs, cancellation (a dropped
request skips an unstarted job), per-call isolation and the timeout are the pool's, unchanged.
`brisvox-server` and the benchmark's engine target (`RealModels`, the `engine-server` child of
`capacity --sim --models`) build it with that one function. Knob: `BRISVOX_SMART_TURN_THREADS`
(server) and `--turn-threads N` (bench), default = the VAD pool's threads (half the cores, 2 on
the 4-CPU container), `0` = the old layout (the analyses share the VAD pool). Without real models
(the mock benchmarks' Smart Turn stub) there is no executor, so those results stand as measured.
Smart Turn outputs are bit-identical (the session configuration is untouched;
`probabilities_are_bit_identical_across_session_configurations` passes). Admission control still
reads the VAD's pool: an overloaded executor degrades end-of-turn quality, it does not refuse calls.

**Options weighed.** (b) A lane rule inside the one pool (analyses may take at most N-1 threads)
gives the same VAD guarantee but ties the analyses' throughput to the VAD pool's size and keeps
warm-up sizing, metrics and the overflow policy mixed; (a) separates them and lets the thread
count follow the measured analysis load. (c) Sizing: pool 2 + executor 2 is 4 pool threads on 4
cores, the VAD ones mostly idle (the analyses' ONNX Runtime intra-op workers come on top). The
executor's size, measured (p95 V2V in ms, the cells of 1-3 runs; `--turn-threads`):

| executor threads | 200 calls | 250 | 300 | 350 | note |
|---|---|---|---|---|---|
| 0 (old) | 889, 1390, 856 | 3514, 2057, 1657 | 3656, 3160 | 3762, 3798 | |
| 1 | **1331** (p50 950) | | | | one thread is 55% busy at 200 calls; verdicts queue when turns end together |
| **2 (default)** | 845, 837, 841 | 838, 840, 841 | 840, 838, 838 | 1592, 1341, 1266 | |
| 3 | | | 1222 | 1734 | 4.4-5.3 cores and 30-34 ms per analysis: oversubscribed, worse |

**A/B, old (`--turn-threads 0`) against new (default), interleaved old/new/old/new, 2 runs each**
(`capacity --sim --models models --pool-threads 2 --server-workers 4 --keep-going --steps
200,250,300,350`; Windows, no CPU limit, the generator on the same machine: provisional; files
`bench/results/capacity-t119-{old,new}-run{1,2}-windows-2026-10-13.*`, the sizing runs
`capacity-t119-sizing-*`). Per cell run 1, run 2:

| calls | p95 V2V old / new (ms) | lost turns old / new | CPU per call old / new (millicores) | mean wait of a VAD window old / new | Smart Turn ms per analysis old / new |
|---:|---|---|---|---|---|
| 200 | 1390, 856 / 837, 841 | 0 / 0 | 11.8, 11.9 / 11.8, 10.8 | 6.2, 6.3 ms / 0.94, 0.86 ms | 24.8, 25.0 / 23.1, 22.8 |
| 250 | 2057, 1657 / 840, 841 | 0 / 0 | 11.8, 11.2 / 10.4, 10.6 | 7.2, 7.1 ms / 0.96, 0.89 ms | 25.8, 25.2 / 22.0, 23.0 |
| 300 | 3656, 3160 / 838, 838 | 0 / 0 | 12.1, 11.9 / 9.9, 9.7 | 8.3, 8.4 ms / 1.03, 0.96 ms | 26.3, 26.1 / 22.5, 22.9 |
| 350 | 3762, 3798 / 1341, 1266 | 97, 109 / 0 | 11.3, 10.9 / 10.9, 10.2 | 8.0, 8.2 ms / 1.06, 1.01 ms | 27.2, 27.0 / 23.5, 24.0 |

The SLO (p95 <= 1200 ms) holds up to 200 calls (old, one of two runs) against 300 (new, both
runs); at 350 the new layout is CPU-bound (3.6-3.8 of the machine's cores in the server, the
generator beside it) with every turn still answered, the old layout loses ~100 turns. A VAD
window waited 6-8 ms on average (max 59-77 ms) behind the analyses and now waits 0.9-1.1 ms (max
7.7-9.4 ms; most of that is the thread wake-up on Windows, 0.05-0.1 ms at 1-10 calls). CPU per
call is the same or up to 20% lower. An analysis takes 2-3 ms less wall time because its two
intra-op workers no longer compete with the VAD jobs. The server CPU limit cannot be set on
Windows: the Linux confirmation is below.

**No regression at low load** (1, 5, 10 calls, 6 calls per slot, 3 runs old/new interleaved;
`bench/results/capacity-t119-lowload-{old,new}-run{1,2,3}-windows-2026-10-13.*`): p50 V2V
822-828 ms (old) against 820-827 ms (new) at every step, p95 842-862 against 844-856 ms, no lost
turn. Smart Turn 18.4-19.5 ms per analysis (old; the first old run 23.6-23.8 ms, a noisy start)
against 18.8-20.5 ms (new). A VAD window waits 46-89 us (old) against 50-53 us (new) at 1 call and
68-132 against 74-80 us at 5 calls. The end-of-turn path is the same code on another thread set
and the end-of-turn delay did not move.

**Linux confirmation to run** (4-CPU/4 GiB container, the equal limits of the capacity benchmark), new layout
(the default): `MODELS=1 RUNS=3 SYSTEMS=brisvox NAME_SUFFIX=-turnexec
STEPS=1,25,50,100,200,300,400,500,600 RESOLUTION=25 bench/capacity_linux.sh`; interleaved with
the old layout: `MODELS=1 RUNS=3 SYSTEMS=brisvox NAME_SUFFIX=-sharedpool
ENGINE_ARGS="--turn-threads 0" STEPS=1,25,50,100,200,300,400,500,600 RESOLUTION=25
bench/capacity_linux.sh` (`ENGINE_ARGS` passes extra `engine-server` flags; each run's
`server.log` has the `vad wait` and `totals smart turn executor` lines). Expected: the knee moves
from ~200 toward the CPU limit (4 cores at ~10-11 millicores per call is ~350-400 calls); if the
container's CPU ends the run, `ENGINE_ARGS="--turn-threads 1"` or `3` shows whether the thread
count matters there.


**Linux A/B (2026-10-13, 4-CPU / 4 GiB container, 3 runs each, interleaved by layout, host busy
with builds of other work).** New layout (executor, `--turn-threads` default): 300, 175, 250 calls,
median **250**; old shared pool (`ENGINE_ARGS="--turn-threads 0"`): 250, 175, 275, median **250**.
No difference within the noise: under the container's CPU quota the gain seen on Windows (no CPU
limit) does not show, and both medians exceed the earlier 200 run, which says as much about the
host's load as about the code. The executor stays (no VAD waits behind analyses, its own
metrics); a quiet-host Linux A/B is in the Smart Turn section above. Files: `bench/results/capacity-brisvox-{turnexec,sharedpool}-models-linux-2026-10-13*`.

## Capacity benchmark (measured 2026-10-13 on Windows; the secondary data)

*Measured before the pacer phase was turned off by default (phase on); not re-measured on Windows.*
`brisvox-bench capacity` (code: `brisvox_bench::capacity`; options and JSON in
`bench/results/README.md`) answers one question about a server: **how many concurrent calls does
it carry while p95 V2V stays within an SLO, with no failed call?** It is the V2V client of the
section above (`run_call`: real-time frames, the same V2V definition, the same fixtures and
ground truth) run many times at once against any websocket URL, so brisvox and Pipecat are
measured by the same code.

**Method.**
- A *step* is N concurrent calls: slot i of N starts `spread x i / N` after the step began
  (4 s by default, so the calls do not all speak in the same instant) and plays the fixture
  (`en_two_utterances`, 2 utterances, 4 s between them, about 16 s per call) `rounds` times (2)
  one call after the other, so N calls are open from the start of the last slot until the first
  slot ends. The step pools the answered turns of all its calls (4N samples).
- A step **passes** when no call failed (could not connect, refused, broke, got an `error`), no
  turn was lost or not clean (`talked_over`, `bot_speaking`, `no_response`; `--max-bad-rate`, 0),
  and **p95 V2V <= floor + 250 ms** (`--slo-over-floor-ms`). The *floor* is the V2V of an
  unloaded turn: for the mock providers the sum of the end-of-turn wait and the providers' fixed
  latencies, **950 ms** (VAD stop 300 + STT final 150 + LLM first token 350 + TTS first byte
  150), stated by the operator for any other server (`--floor-ms`, from a one-call `v2v` run).
  The SLO is therefore 1200 ms here. The ramp stops at the first step that does not pass; the
  reported capacity is the largest passing step *below* it, and `--resolution N` bisects between
  that step and the failing one until they are N apart.
- A step in which the load generator sent more than 1 % of its frames (`--client-late-share`)
  more than 100 ms late (`--client-lateness-ms`) and that is otherwise healthy is *generator
  limited*: the numbers then describe the generator, not the server, and the ramp ends with a
  lower bound. One late frame (a scheduler hiccup) decides nothing; and a step with failed
  calls, lost turns or a p95 over the SLO stays a *fail* even when frames were late as well
  (a stalled server backs up the generator's own writes, which also reads as lateness), with the
  lateness named in its reasons. The generator's own CPU is sampled next to the server's, and the
  late-frame count and the largest send lateness are in every row.
- One warm-up call is played first and not counted (a Python process otherwise fails its first
  calls on lazy imports; the Pipecat bot also builds one pipeline per process at startup).
- The server is a process of its own, and with `--pid` its CPU time and resident memory are
  read, summed over the process tree (Pipecat's worker processes, the Windows venv launcher),
  every 500 ms on a thread of its own. On Linux the sampler also reads the PSS (`smaps_rollup`,
  summed over the tree) as `pss_peak_bytes`. CPU "cores" are CPU seconds per wall second over the step.

**What runs on each side** (no keys, no provider noise):

| | brisvox | Pipecat 1.12.0 |
|---|---|---|
| process | `brisvox-bench engine-server` (started by `capacity --sim`): the production engine (`run_call`, the transport's pacer, the inference pool) behind the real websocket server | `bench/pipecat_baseline/bot.py --mock-providers`: FastAPI/uvicorn, one real Pipecat pipeline per call; `--workers N` for N processes |
| providers | `brisvox_sim` mock STT / LLM / TTS at the trait boundary | `mock_providers.py`: a real `STTService`, the real `OpenAIResponsesHttpLLMService` with only its HTTP stream replaced, a real `TTSService`: same fixed latencies and shapes (table in `bench/pipecat_baseline/README.md`; every shared number is checked against the Rust source by a test) |
| VAD, turn | the production VAD worker over an energy model (RMS 500, start 40, stop 300 ms), Smart Turn feature extraction real, model fixed "complete" | an energy `VADAnalyzer` on the same 32 ms windows (10 windows to stop), Smart Turn v3 features real, model fixed "complete" |
| floor | 950 ms | 950 ms |

What this measures, and what it does not. It is the cost of the framework: orchestration, frame
passing, the VAD and turn plumbing, the output pacing, the websocket. It is **not** the cost of
the models: the Silero VAD and the Smart Turn ONNX model are not run on either side (the bench
build has no Silero), so a real deployment needs more CPU per call than any row below,
and a system that spends more in the models will rank differently once they are on. A live run
with real providers (the live phase) adds the network and the providers' own limits. The sim
server has no admission control: a step fails when the engine degrades, not when it
refuses calls.

**Results, Windows, 2026-10-13: a trend, not a Linux run (the Linux numbers are in the section above).** The files are
`bench/results/capacity-{brisvox,brisvox-pool4,brisvox-queue1024,brisvox-queue1024-calls2048,pipecat-1proc,pipecat-4proc}-windows-2026-10-13.{md,json}`
(each starts with the command and the "not a Linux run" banner, and the JSON carries the same in
its `environment` block; the Pipecat files also state how the bot was started).

**Correction after review: the first brisvox rows were limited by the benchmark's own queue.**
The sim engine server built its inference pool with a 256-job queue where production
(`PoolConfig::default`) has 1024. The files `capacity-brisvox-` and `capacity-brisvox-pool4-`
were measured with that 256 queue (their JSON states `engine.pool_queue: 256`), and their
cliffs at 450 and 1000 calls were largely that queue filling, not the engine: with the
production queue (`--pool-queue 1024`, now the default) the same 2-pool-thread server passes
1000 calls (the ramp 50, 200, 400, 600, 800, 1000 all pass; in the first run of it 450, 500
and 600 passed as well), and with the call limit raised from 1024 to 2048 (`--max-calls`) it
passes 1200 and loses turns at 1500. The 256-queue files are kept as the ablation that shows the
queue was the limit, and are not the capacity figure. The machine is a Windows 11 development PC with 32 logical cores,
shared with other builds, running the load generator, the server and nothing else of ours; no
container limit applies to either server. To give the two systems the same budget as a 4-core
container, brisvox runs 4 tokio workers and 2 (or 4) pool threads, and Pipecat runs 1 process
(the default setup: one event loop) and 4 processes. Release build, Python 3.13.14, Pipecat
1.12.0, fixture `en_two_utterances`, 2 calls per slot, one warm-up call, SLO p95 V2V <= 1200 ms,
4 generator workers. The generator's largest send lateness was 17-26 ms (the Windows timer
tick) in the 256-queue and Pipecat files and up to 41 ms above 1000 calls, far below the 100 ms
limit: no step was generator limited (the late-frame counts of the schema-2 files are 0; the
four older files predate the counts and carry only the largest lateness).

| system | server | max concurrent calls within the SLO | first step that did not pass | p95 V2V at the max | server CPU at the max | CPU per call | RSS peak at the max |
|---|---|---:|---|---:|---:|---:|---:|
| brisvox (2 pool threads, queue 1024, 1024 calls allowed) | 1 process: 4 tokio workers, 2 pool threads, production queue | **1000** | 1100: 152 calls refused (the sim server's own 1024-call limit), p95 1033 ms | 1032 ms | 1.88 cores | 1.9 mcores | 768 MB |
| brisvox (2 pool threads, queue 1024, 2048 calls allowed) | same, `--max-calls 2048` | **1200** (ramp 1000, 1200, 1500) | 1500: 1093 of 6000 turns without an answer, p99 3.6 s | 1041 ms | 2.08 cores | 1.7 mcores | 912 MB |
| brisvox (2 pool threads, queue 256, ablation) | same as the first, benchmark's 256 queue | 400 | 450: 9 of 1800 turns lost | 1015 ms | 0.69 cores | 1.7 mcores | 320 MB |
| brisvox (4 pool threads, queue 256, ablation) | 4 pool threads, 256 queue | 900 | 1000: 1005 of 4000 turns lost, p95 2359 ms | 1029 ms | 1.91 cores | 2.1 mcores | 725 MB |
| Pipecat, 1 process | 1 uvicorn process (one event loop) | **50** | 55: p95 1211 ms | 1090 ms | 0.93 cores | 18.5 mcores | 248 MB |
| Pipecat, 4 processes | 4 uvicorn worker processes | **160** | 170: p95 1468 ms | 1086 ms | 3.49 cores | 21.8 mcores | 968 MB |

At the same load (p95 V2V of the answered turns and server CPU in cores; the full ladders are
in the result files):

| concurrent calls | brisvox (2 pool threads; 256 queue up to 200 calls, 1024 queue from 400) | Pipecat, 1 process | Pipecat, 4 processes |
|---:|---:|---:|---:|
| 1 | 1033 ms, 0.02 | 1066 ms, 0.05 | 1068 ms, 0.05 |
| 10 | 1030 ms, 0.04 | 1064 ms, 0.19 | |
| 40 | | 1064 ms, 0.71 | 1061 ms, 0.78 |
| 50 | 1032 ms, 0.07 | 1090 ms, 0.93 | |
| 60 | | 2863 ms, 1.03, 5 of 240 turns lost | |
| 100 | 1034 ms, 0.28 | | |
| 120 | | | 1058 ms, 2.63 |
| 160 | | | 1086 ms, 3.49 |
| 200 | 1025 ms, 0.48 | | 1986 ms, 4.00 |
| 400 | 1017 ms, 0.68 | | |
| 1000 | 1032 ms, 1.88 | | |

What the numbers say, and how far to trust them:
- **Capacity.** With the production inference queue brisvox carried 1000-1200 calls
  (Windows, 2 pool threads, mock providers) against 50 for one Pipecat process and 160 for four,
  with the same SLO, no failed call and no lost turn: 20-24 times one Pipecat process and 6-7.5
  times four of them, on 1.9-2.1 cores against 0.9 (one process, saturated) and 3.5 (four).
  The first version of this sentence said 8-9 times (400-450 calls): that figure measured the
  benchmark server's own 256-job queue (see "Correction after review" above) and is withdrawn.
  (The optimization pass moved the 1200 / 1500 limit to 1800 / 2000 with a high-priority line for the VAD
  batches: "Optimization pass" below.)
  The two queue-1024 ramps agree on 1000 passing; the limit between 1200 and 1500 is one run
  and the repeat spread of the other rows (about one step, once more) applies to it too. The
  1000 and 1200 rows are also near what one Windows generator can hold (it reported
  `os error 10048`, address in use, for a few connects at 1200 in the 1024-call run, and used up
  to 1.5 of its 4 worker cores), so they are lower bounds for the engine.
  Per call, brisvox spent 1.7-2.1 millicores at its limit and Pipecat 18.5 (one process) to
  21.8 (four processes): 9 to 13 times more. The absolute figures are a lower bound for both
  (no models run, see above) and Windows only gives a trend. **Between runs the limits moved by
  about one step, and once by more**: brisvox with 2 pool threads and the 256 queue, 400, 450 and 400 in three
  runs (first failure 500, 500, 450), with 4 pool threads 700 and 900 (first failure 800,
  1000); Pipecat with one process 50 and 50 (first failure 55, 55; an exploratory probe before
  the bot pre-warmed its pipeline, one call per slot, had failed at 40); Pipecat with four
  processes 110, 160 and 160 (the first run's file was overwritten by a rerun with the
  `--note` flag; it stopped at 120 with p95 3430 ms). A shared Windows desktop is the likely
  reason; the Linux run needs at least three repeats and medians.
- **Latency at one call.** The floor plus 60 ms for brisvox (p50 1011 ms), plus 90-110 ms for
  Pipecat (1040-1061 ms in the runs). That includes the websocket, the client and the 32 ms
  VAD windows on both sides (the v2v mock-bot self-check above puts harness and transport at
  about 30 ms).
- **Memory (resident set of the process tree, peak).** brisvox 20 MB with one call and about
  0.75-0.8 MB per open call (320 MB at 400 calls, 768 MB at 1000, 912 MB at 1200). Pipecat 141 MB with one call
  and about 2.2 MB per call (248 MB at 50); four processes start at 572 MB and add about 2.5
  MB per call (968 MB at 160).
- **How they fail.** Pipecat's single event loop saturates at one core (0.93 at 50, 1.03 at
  60): p95 goes from 1.09 s at 50 calls to 1.2 s at 55 and 2.9 s at 60 (the first run: 10.9 s
  and 61 of 240 turns lost), a cliff within 20 % of the load. Four processes do not scale four
  times (160, not 200): the OS spreads the connections over the workers without balancing them,
  so the busiest worker probably saturates first (not checked per worker); at 180 calls 46
  calls failed (a websocket write timed out). brisvox fails differently: the p95 of the *answered* turns stays at
  1.02-1.06 s while turns are *lost* (no audible answer within the response window). With the
  benchmark's old 256-job queue that began at 450 calls (9 of 1800 turns, 10 of 2000 at 500; at
  600 half of them) and moving from 2 to 4 pool threads moved the cliff to about 950: that
  was the queue (and the pool draining it) giving way early, not the engine's CPU (0.7 of 4
  cores busy). Ablation, 2 pool threads, queue 256 against 1024: 400-450 against 1000-1200
  calls, so the queue size alone moved the cliff by a factor of 2.5-3 (the 4-thread, 1024-queue
  combination was not re-run). With the 1024 queue the failure at 1500 calls is again turns
  without an answer (1093 of 6000) with a p99 of 3.6 s, at 2.25 of 4 server cores: the
  pool and the VAD stop being decided late is the likely cause, not isolated here. The sim
  server has no admission control beyond its call limit (a refused call at the limit is a failed
  call: the 1100 step of the 1024-call run); the real server sheds new calls on pool pressure
  first, and with the Silero model each window costs more, so the real cliff is
  lower, and the pool size is the first knob to look at.
- **Not measured here.** Linux is in the section above (the cliff positions move with the
  scheduler and the timer; here the generator and the server share a Windows desktop), the models, real providers and their
  rate limits, other fixtures (Turkish, tool calls), calls that interrupt, and a Pipecat tuned
  beyond the default (a load balancer in front of the processes, or a Smart Turn executor per process).

**Readings.** CPU is process CPU time over wall time between the first and last 500 ms readings
inside the step; on Windows the kernel accounts CPU in scheduler ticks, so a light server (a few
percent of a core) reads noisily: brisvox at 50 calls read 0.20, 0.13 and 0.07 cores in three
runs. The large readings (the Pipecat rows, brisvox above 200 calls) repeat within 15 %. The
generator's own CPU is in the files (up to 1.1 cores at 1000 calls).

**Reproducing it on Linux with the same container limits** (`bench/capacity_linux.sh`; run on
2026-10-13, results in the section above; the commands below are what it does by hand):

```sh
# 4 CPUs and 4 GiB for each server, pinned to cores 0-3; the load generator on cores 4-7
CPUS=4 MEM=4g bench/capacity_linux.sh
# what it does, by hand, per system:
docker run -d --name srv --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 \
  --network host -v "$PWD":/src:ro debian:bookworm-slim \
  /src/target/linux/release/brisvox-bench engine-server --bind 0.0.0.0:8080 \
  --fixtures-dir /src/fixtures --max-calls 1024 --pool-threads 2          # brisvox
docker run -d --name srv --cpus=4 --cpuset-cpus=0-3 --memory=4g --memory-swap=4g --ulimit nofile=1048576:1048576 \
  --network host -v "$PWD":/src:ro -v brisvox-pcvenv:/venv:ro python:3.13-slim-bookworm \
  /venv/bin/python /src/bench/pipecat_baseline/bot.py --mock-providers --host 0.0.0.0 \
  --port 8080 --workers 4                                                  # Pipecat, 4 processes
taskset -c 4-11 target/linux/release/brisvox-bench capacity --url ws://127.0.0.1:8080/ --fixtures-dir fixtures --client-workers 8 \
  --pid "$(docker inspect -f '{{.State.Pid}}' srv)" --floor-ms 950 --label NAME \
  --steps 1,5,10,20,40,80,120,160,240,320,480,640 --resolution 10 --rounds 2 \
  --out-md bench/results/capacity-NAME-linux-DATE.md --out-json bench/results/capacity-NAME-linux-DATE.json
```

The same limits for both: `--cpus` (the CFS quota; Rust's `available_parallelism` and so the
tokio worker count follow it; `--pool-threads` is half the CPUs, as `PoolConfig::default` has it),
`--cpuset-cpus`, `--memory`; the
Pipecat bot gets one worker process per CPU (it is one event loop per process; a single process
is also in the table). Run the two one after the other on the same host, repeat each at least
three times, and compare medians. Check before the run that the generator is not the limit (its
CPU column and send lateness) and that the host has CPUS + 4 free cores.

## Optimization pass (measured 2026-10-13 on Windows)
Full write-up with the profiling method, the counters and every file:
`bench/results/2026-10-13-optimization.md`. Windows, mock providers, energy VAD and a fixed
Smart Turn model, 4 tokio workers and 2 pool threads: a trend, not a Linux run.

- **What limited the capacity run.** The 1500-call step of the first capacity ramp (1093 of 6000 turns
  lost) was not CPU: the pool was 30-43 % busy. Smart Turn analyses (about 400 a second at 1.5
  ms each in the sim, 22.5 ms with the model) shared one FIFO queue with the ~75 000 VAD batches
  a second (one per call and 20 ms frame); when the turns of a few hundred calls ended together
  the batches waited behind the analyses, the 1024-job queue filled and rejected them, and a
  rejected batch loses the call's VAD stream. Without Smart Turn (`--engine-turn fixed:300`) the
  same step passes with the pool 3-4 % busy.
- **The fix.** The pool has two bounded lines (`Priority::High` for the VAD batch, `Normal` for
  the rest); a worker takes a waiting high job first. The largest step within the SLO moved from
  1200 to 1800 calls (p95 1168 ms), and past it the server degrades through the p95 (2000 calls:
  1224 ms, every turn answered) instead of losing turns (1500 calls: 18 % lost). `brisvox-server`
  sizes the queue `max(1024, 2 x max_sessions)`.
- **What limits it now.** The four tokio workers (82 % busy, 250 000-350 000 polls a second, a
  third of their time in the kernel's loopback send / receive); 8 workers pass 2200 calls. Per
  call the CPU did not change (1.7 mcores): the pass moved the cliff.
- **V2V at one call is unchanged** (p50 1009.4 -> 1006.9 ms, p95 1028.0 -> 1034.2 ms, run noise).
- New timeline events `FirstChunk` (segmenter) and `TtsFirstText` (TTS stage), once per response;
  the V2V stage `first_chunk_to_tts_text` (the hop between the two stages) is 0.007 ms p50.

### Optimization pass follow-ups
- **A refused VAD batch no longer loses the call's VAD.** The pool refusing a batch (or a queue
  timeout before the job started) means the job never ran, so the worker keeps the stream and the
  audio, waits 10 ms and offers them again together with the frames that arrived meanwhile
  (`VadStatsSnapshot::retries`). Overload becomes delay of the stop/start events, not a lost turn
  and a rebuilt stream plus a synthesized stop. Tests: `vad_worker`
  `a_saturated_pool_delays_the_batch_and_keeps_the_stream_and_the_turn` (speech still starts at the first, refused frame,
  `rebuilds` 0) and `audio_kept_for_a_retry_is_bounded_and_shed_like_a_tee_overflow`. Still
  counted toward the fail-open streak (5 refusals over 1 s of audio). Not re-measured at 1800-2000
  calls on this machine (RAM shared with other builds): the capacity ramp on Linux is the job for that.
- **Incremental Smart Turn features: not adopted.** The Whisper extractor normalizes the waveform
  with the mean and variance of the whole 8 s window before any frame is cut (bit-exact with numpy
  float32), so sliding the window by even 100 ms changes every sample, hence every
  column. Test `a_slid_window_changes_every_shared_column_so_columns_cannot_be_reused` slides
  the window by 10 hops and finds 0 of 63 200 shared values identical (largest difference 0.052, against the
  1e-6 that already moves the model). Reusing columns would
  break the bit-exact goldens (the int8 model moves by 0.13 for 1e-6 on the features), and a
  mathematically equivalent rewrite (FFT of the raw frame, scaled afterwards) is not bit-exact
  either. The upside is small: features are ~1.0-1.2 ms against the 22.5-28 ms model
  ("Smart Turn"), so at most ~4 % of an analysis. The current extractor stays.
- **`brisvox-bench engine-server` sizes the pool queue like `brisvox-server`**: without
  `--pool-queue` it is `max(1024, 2 x max-calls)`; an explicit value is used as given, and the
  report shows the capacity actually used.

## Targets (hypotheses until the benches measure them)
- Internal overhead p99 < 10 ms at 300 concurrent simulated sessions on 4 cores.
- V2V median lower than Pipecat with identical providers and equivalent turn settings, with
  speculation off; further reduction with speculation on. Publish exact numbers, not claims.
  (Over mock providers: 60 ms against 90-110 ms above the 950 ms floor at one call, "Capacity
  benchmark"; with live providers: the live phase.)
- Capacity at p95 V2V <= floor + 250 ms: more calls per container than Pipecat with the same
  CPU limit. Linux, equal 4-CPU/4 GiB containers (2026-10-13, WSL2, 3 runs each): brisvox 2100
  (2100-2200) against 90 (70-90) for Pipecat with four tuned processes and 70 with its defaults
  ("Capacity benchmark, Linux"). Windows evidence (trend only): 1000-1200 against 50 (one
  process) and 110-160 (four); an earlier 400-450 was the benchmark's own 256-job queue.
