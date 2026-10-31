# Benchmark results

Committed output of `brisvox-bench` (`crates/brisvox-bench`). A result file starts with the command
that produced it, the build, the machine and the setup; regenerate with that command. The binary
refuses to write a result (and exits non-zero) if the run was invalid, so a committed file is a
complete run.

| File | Command | What |
|---|---|---|
| `sim-baseline.md`, `sim-500-1worker.md` | `sim-load` | in-process load, N simulated calls |
| `sim-baseline-linux.md`, `sim-saturation-linux-run{1,2}.md`, `sim-saturation-linux-first.md` | `bench/sim_load_linux.sh` (`taskset -c 0-3 sim-load ...`) | the same on Linux (WSL2) confined to 4 cores, and a sweep up to 12000 calls; `-first` is a noisy first sweep kept for honesty |
| `fixtures-sim.md` | `fixtures` | whole simulated calls on real speech, server-side |
| `inference-pool.md`, `resampler.md`, `smart-turn.md` | `infer-load`, `resample` | component costs |
| `denoise-cpu.md` | `denoise cpu` (feature `denoise-fastenhancer`) | CPU and delay per call of each noise suppression backend at 16 kHz and 8 kHz |
| `denoise-quality.md`, `denoise-quality-musan-background.md` | `denoise quality --models DIR [--noise-dir DIR]` | false barge-ins, missed and fragmented utterances and Smart Turn verdict changes with real noise at 20/10/5/0 dB, wideband and 8 kHz mu-law, with and without each backend; the first uses the committed `fixtures/noise` recordings, the second 34 MUSAN background recordings (list in `docs/FIXTURES_SOURCES.md`); method and reading in `docs/LATENCY.md`, "Noise suppression" |
| `noise-handling-calls.md` | `denoise calls --set all --models DIR --levels` | false and missed barge-ins after the engine's real guards, with the bot speaking, for background speech, bot echo and noise at several levels, wideband and 8 kHz mu-law, per front-end option (guards, level gates, VAD parameters, denoisers); method and reading in `docs/LATENCY.md`, "Noise handling beyond a denoiser" |
| `denoise-safety.md`, `denoise-safety-forced.md` | `denoise safety [--force]` | clean speech through each backend on both paths at three caller levels: gain, VAD recall, missed utterances; the second ignores the engine's refusal of a backend below its minimum rate and shows what Hush does to 8 kHz audio |
| `denoise-wer-sherpa-en-hush.md` | `denoise transcribe` against the local sherpa-onnx recognizer | word error rate with and without Hush, wideband and telephone |
| `denoise-cpu-hush.md`, `denoise-quality-hush.md`, `denoise-quality-hush-musan-background.md` | `denoise cpu`, `denoise quality` with `--backends none,rnnoise,hush` | CPU of Hush, and the VAD-level false starts, fragments and Smart Turn flips with Hush |
| `v2v-sim.md`, `v2v-sim.json` | `v2v --sim` | voice-to-voice over the websocket, the engine over mock providers |
| `v2v-mock-bot.md`, `v2v-mock-bot.json` | `v2v --sim --target mock-bot` | the same against the mock bot: harness + transport self-check |
| `speculation-fixtures.md` | `speculation` | speculative LLM start on the fixtures, paused clock, deterministic: V2V, run hit rate and turns committed, extra requests and wasted tokens per turn, per end-of-turn policy, with a 350 ms and a 100 ms LLM |
| `v2v-speculation.md`, `v2v-speculation/` | `v2v --sim --speculation ...` | the same over the websocket in real time, speculation off/on x turn policies |
| `v2v-speculation-gate.md`, `v2v-speculation-gate/` | `v2v --sim --speculation --speculation-gate-ms N ...` | the stable-partial gate over the websocket in real time: what waiting N ms before offering the partial costs on continuous speech; the resumed-speech scenario is in `speculation-fixtures.md` |
| `capacity-<system>-cloud-<N>c[-max]-linux-<date>[-run<N>].md`, `.json` | `brisvox-bench capacity --url ... --floor-ms 860 [--slo-over-floor-ms 60000]` against a server container, `bench/capacity_summary.py` | the capacity comparison the README shows: each system's real provider client code against `bench/fake_providers` over TLS, the real Silero VAD and Smart Turn, cloud containers on AMD EPYC 9655P with `<N>` vCPU / `<N>` GB; without `-max` the SLO ramp (p95 at most 1110 ms), with `-max` the most calls with no failed call or lost turn at any latency; `steps[].target` holds the server container's CPU and memory (PSS) per step from its cgroup samples; method in `docs/LATENCY.md`, "Capacity with the real provider path" |
| `capacity-<system>[-8c8g][-max]-fakes-models-linux-<date>[-run<N>].md`, `.json`, `.server.log` | `PROVIDERS=fakes MODELS=1 bench/capacity_linux.sh` | the same comparison in local Docker containers (a desktop host; its single memory channel limits the turn model at 8 vCPU, see `docs/LATENCY.md`) |
| `capacity-<system>-<platform>-<date>.md`, `.json` | `capacity --sim`, `capacity --url ... --pid ...` | the capacity ramp: max concurrent calls within a p95 V2V SLO, with CPU and memory of the server process; one file per system, brisvox and Pipecat over mock providers |
| `v2v-live-<system>-c5-windows-<date>.md`, `.json`; `v2v-live-c5-cartesia-limit-windows-<date>.md` | `v2v --url ... --concurrency 5` against brisvox-server / the Pipecat baseline with `agent_live_en_aura.toml` | the live comparison with 5 concurrent calls and 100 turns per system, Deepgram Aura voice, OpenRouter pinned to one upstream; the `-cartesia-limit-` note records why the Cartesia attempt stopped (its plan allows 2 concurrent generations) |
| `v2v-live-<system>[-tr]-windows-<date>.md`, `.json` | `v2v --url ...` against brisvox-server / the Pipecat baseline on real providers (Deepgram, OpenRouter, Cartesia) | the live V2V comparison against Pipecat: V2V percentiles, the provider latencies each system's own timers saw, the residual, every turn and the failures; generated by `scripts/bench_live_report.py`; summary in `docs/LATENCY.md` |
| `capacity-<system>-linux-<date>.md`, `.json`, `capacity-<system>-linux-<date>-run<N>.md`, `.json` | `bench/capacity_linux.sh`, `bench/capacity_summary.py` | the Linux capacity ramps in equal 4-CPU containers: `<system>` is `brisvox`, `pipecat` (defaults), `pipecat-tuned` (BLAS threads pinned to 1) or `brisvox-nofile1024` (ablation: the container's default 1024 file-descriptor limit); the summary is the per-run table and the median, `-run<N>` the ramps |
| `capacity-brisvox-models-<platform>-<date>[-run<N>].md`, `.json`, `.server.log` | `capacity --sim --models`, `MODELS=1 bench/capacity_linux.sh` | the capacity ramps with the real Silero VAD and Smart Turn v3.2: `-windows-` files are provisional trend data without a CPU limit (`-pool4-` is the 4-pool-thread ablation); the `.server.log` is the engine server's pool stats and Smart Turn / VAD CPU split. Linux runs also carry `pss_peak_bytes` / `pss_before_bytes` (PSS summed over the process tree) and the markdown a PSS column; `capacity-pipecat-q-tuned-models-linux-2026-10-14*` is the PSS re-measure of Pipecat, `capacity-brisvox-q-pss-crosscheck-models-linux-2026-10-14*` the one-run brisvox cross-check (not in the charts) |
| `capacity-livekit-models-linux-<date>[-run<N>].md`, `.json`, `.turns.jsonl`, `.agent.log`; `capacity-livekit-default-models-linux-<date>[-run<N>]...`; `capacity-livekit-sfu-<date>.md` | `bench/livekit_baseline/capacity_livekit.sh`, `bench/capacity_summary.py` | LiveKit Agents 1.8.7 with Silero VAD and its own turn detector over mock providers in the same 4-CPU / 4 GiB container: the tuned variant (turn detector threshold lowered, model still runs) and the defaults; memory is PSS; the SFU's CPU and memory, outside the budget, in the `-sfu-` file; methodology in `bench/livekit_baseline/README.md` |
| `2026-10-13-optimization.md` | `capacity --sim ... --stats-every-ms`, `v2v --sim` | the optimization pass: what limited the capacity run, what was changed, before and after. Its files: `capacity-brisvox-priority-*`, `capacity-brisvox-t083-{before,ablation-fixed-turn}-*`, `v2v-sim-t083*.{md,json}` |

## Voice-to-voice benchmark

```text
brisvox-bench v2v --sim  [options]              # the engine over mock providers, in process
brisvox-bench v2v --sim --target mock-bot       # the mock bot instead (harness self-check)
brisvox-bench v2v --url ws://HOST:PORT/PATH     # any server (live mode); or BRISVOX_BENCH_URL
  --fixtures a,b        fixtures in fixtures/ (default: all three committed ones)
  --runs N              calls per fixture (default 10); percentiles are over all of them
  --min-gap-ms 4000     pad silences between utterances to at least this (see below)
  --response-timeout-ms how long after the fixture the last turn may still be answered (10000)
  --silence-peak 200    bot audio below this peak amplitude (of 32768) is silence
  --strict              live mode: a bot_speaking turn also invalidates the run (sim mode is
                        always strict; talked_over and no_response always invalidate it);
                        engine target: an engine stage over its p95 budget invalidates it too
                        (without --strict it is only a warning)
  --out-md FILE --out-json FILE
  --engine-turn smart|fixed:MS --vad-threshold 500 --vad-start-ms 40 --vad-stop-ms 300
  --stt-final-ms 150 --llm-first-token-ms 350 --llm-inter-token-ms 30 --tts-first-byte-ms 150
  --pool-threads 2
                        engine target: end-of-turn policy, energy VAD, mock provider latencies
  --barge-in-runs N --barge-in-gap-ms 1600 --memory-calls 8
                        engine target: barge-in calls after the latency runs (default: as many
                        runs as --runs, 0: none), and calls played at once for memory (0: none)
  --speculation | --speculation-final-only   --speculation-gate-ms 0   --stt-interims lagging|converged
                        engine target: speculative LLM start (the second form only at the
                        STT final, not from the partial at the VAD stop) and how close the mock
                        STT's last interim is to its final (lagging: a word short, the mock's own;
                        converged: the full text). The report gains a Speculation section.
                        --speculation-gate-ms N: the stable-partial gate, the partial
                        of a VAD stop is offered only after N ms of silence with unchanged text.
  --bot-endpoint-ms 500 --bot-delay-ms 300 --bot-jitter-ms 0 --bot-reply-ms 600 --bot-seed 1
                        mock-bot target: the mock bot's behaviour
```

The client plays each fixture in real time (20 ms frames, deadlines on the injected clock), over
the websocket protocol of `docs/WEBSOCKET_PROTOCOL.md`, one call at a time, and records when the
bot's audio arrives. It holds no provider keys and sends no credentials. A live URL that carries a
token is never printed: reports show only `scheme://host:port` (a hidden path is shown as
`/...`), the command line in the report header is redacted the same way, and the URL is a
`Secret` in the library, so it is not in `Debug` output or error text either.

### Definitions

- **EOS** (end of speech): ground truth from the fixture sidecar (`fixtures/<name>.json`),
  as an offset from the start of the stream. After gap padding the offsets are those of the padded
  stream (the report lists them under "Per turn").
- **V2V**: arrival at the client of the first non-silent bot audio frame after EOS, minus EOS
  (`docs/LATENCY.md`). Audio frame granularity is the server's (normally 20 ms); a frame is
  non-silent if its peak sample reaches `--silence-peak`.
- A frame is sent once its last sample has been spoken (`begin + k * 20 ms`, absolute deadlines),
  so nothing leaves before it was uttered. Send lateness is measured and reported
  (`max_send_lateness_us`); a late final speech frame inflates that turn's V2V by as much. On
  Windows timers tick at ~15.6 ms, so expect up to ~16-25 ms there; Linux is ~1 ms. Use Linux
  numbers for any comparison.
- **Outcome per turn**: `answered` (a clean sample, the only ones in the percentiles),
  `talked_over` (the first audio arrived after the user began the next utterance), `bot_speaking`
  (audio was already flowing within 100 ms before EOS), `no_response`. They are counted next to
  the percentiles, so a slow or silent bot cannot hide. `talked_over` and `no_response` turns are
  the slowest ones, so any of them makes the result unfit to commit in every mode (the command
  prints the report and the problem and writes neither file): percentiles without them would cut
  the tail.
- After the fixture the client keeps sending digital silence every 20 ms until the last turn is
  answered or the response window ends, as an open phone line does: a server whose end of turn
  counts silence from incoming frames (VAD, smart turn, STT endpointing) still ends the last turn.
- Percentiles are nearest rank over answered turns (p99 is the maximum below 100 samples).
  Failed calls (connection broke, server error, server hung up early) are listed and excluded; the
  benchmark stops after three failed calls in a row.
- **Padding.** The recordings leave 1.0-1.5 s between utterances, less than a bot needs to answer.
  Played as recorded, the caller would talk over every answer. `--min-gap-ms` widens every gap to
  at least that much digital silence (speech untouched), the same as `fixtures`.

### Sim mode and what it measures

Sim mode serves **the engine** in process behind the real `WebSocketServer`/`TransportSession` of
`brisvox-transport` (`brisvox_bench::v2v::engine_target`): every call runs the production
assembly `brisvox_engine::run_call` (pump, `AudioIn` + VAD worker, STT stage, turn driver, LLM
runner, segmenter, TTS stage, `TransportOut`, the output pacer) with its heavy work on a real
inference pool. Only the providers are mocks (`brisvox_sim`), with **fixed** latencies: the mock
STT answers each `Finalize` with the fixture's transcript after `--stt-final-ms`, the mock LLM
answers each turn with one short sentence in the utterance's language after
`--llm-first-token-ms`, and the mock TTS speaks it after `--tts-first-byte-ms`. Not production:
the VAD is the simulator's energy detector run by the production VAD worker (Silero is a
`brisvox-inference` feature the bench does not build), and with `--engine-turn smart` (default,
the engine's adaptive policy) the Smart Turn model always answers "complete" (its feature
extraction is real). The harness tells the server which fixture the next call plays (the mock STT
needs its transcripts) and waits after every call until the server has torn it down; anything
the engine records (a pipeline error, a call without a script, a call that does not end within
10 s) fails that call in the report.

Every turn then has a known floor, stated in the report: **end of turn** = VAD stop
(`--vad-stop-ms`) + STT final (the turn driver holds the end for the final), or the fixed silence
if longer; **latency floor** = end of turn + LLM first token + TTS first byte (the answer,
`Okay, got it.`, comes in one token, and its first clause `Okay,` is a first chunk of its own,
decided inside the token: the engine's first TTS chunk ends after one word; before that change
it needed three words and waited one token gap for the response's end). What the
measurement shows above the floor is the engine's own time (VAD windows, pool hops, stage queues,
the 20 ms frame grid, the output pacer's phase) plus the websocket transport and the harness;
`v2v-mock-bot.md` measures the last two alone.

**Engine stages and memory.** Against the engine the report adds an "Engine stages"
section (JSON `engine_stages`): the engine's own per-stage times from its turn timeline
(`TimelineRecorder`, stamped where the work happens), with the mock provider time inside each span
and the engine-overhead budget at p95 (`docs/LATENCY.md`, "Engine stage times and memory"):
`vad_stop_to_finalize`, `turn_end_to_llm_request` (budget 3 ms), `llm_request_to_first_token`
(provider), `first_token_to_tts_text`, `tts_request_to_first_byte` (provider),
`tts_audio_to_transport` (budget 2 ms), `interrupt_to_clear` (budget 1 ms) and
`turn_end_to_first_audio`. The provider spans show how precisely the mocks keep their fixed
latencies (a sleep overshoots by the timer's precision, up to a ~15.6 ms tick on Windows). The latency calls give every stage but the clear. After them,
`--barge-in-runs` runs play every fixture with its gaps set to exactly `--barge-in-gap-ms` while the
mock LLM answers at length (`kind: "barge_in"`): the caller talks over the bot, the engine
confirms a barge-in and clears, and these calls give `interrupt_to_clear` and nothing else; a run
of barge-in calls in which the engine confirmed no barge-in is a problem. Last, `--memory-calls`
calls of the first fixture run **at once** (`kind: "memory"`): resident memory is read with the
server cold, again idle and warm right before them, and sampled every 50 ms while they run; RSS per
call = (peak - warm idle) / calls. One call at a time cannot show a call's footprint (the next call
reuses what the previous one freed). Barge-in and memory calls never enter a V2V aggregate.

`--target mock-bot` serves the mock bot instead (the original target of the benchmark harness): energy end-of-turn
detection (`--bot-endpoint-ms` of silence), a simulated provider delay (`--bot-delay-ms` plus
seeded jitter), then a tone reply. Its expected result is the floor `endpoint + delay` plus about
one to two 20 ms frames and the loopback transport; a result far above that means the harness or
the transport is adding latency.

### JSON format (`schema: "brisvox-bench/v2v/3"`)

All durations are whole microseconds in fields ending in `_us`.

```text
schema, mode ("sim" | "live"), target (redacted URL)
settings: runs, fixtures[], min_gap_ms, frame_ms, silence_peak, overlap_guard_ms, response_timeout_ms
engine: null | { end_of_turn ("smart_turn" | "fixed_silence"), turn_silence_ms (null | ms),
    vad_threshold_rms, vad_start_ms, vad_stop_ms, stt_interim_ms, stt_final_ms,
    llm_first_token_ms, llm_inter_token_ms, tts_first_byte_ms, tts_chunk_interval_ms,
    pool_threads, end_of_turn_floor_ms, floor_ms }
mock_bot: null | { endpoint_silence_ms, response_delay_ms, response_jitter_ms, reply_ms }
runs[]: one per call
    fixture, run, call_id, frames_sent, audio_frames_received, audible_frames, clears,
    server_errors[], max_send_lateness_us, failure (null | text; failed calls are excluded below),
    kind ("latency" | "barge_in" | "memory"; only latency calls enter the aggregates)
    turns[]: index, lang, eos_us, outcome, v2v_us (null unless answered or talked_over)
failed_runs, max_send_lateness_us
overall, per_fixture[], per_language[], per_turn[]: counts and the answered V2V distribution
    turns, answered, talked_over, bot_speaking, no_response,
    v2v_count/p50_us/p90_us/p95_us/p99_us/max_us (as count, p50_us, ...)
engine_stages: null | { stages[]: { name, from, to, provider_us, budget_p95_us (null | us),
    within_budget (null | bool), count, p50_us, p90_us, p95_us, p99_us, max_us },
    turns, interrupts, dropped,
    rss: null | { cold_bytes, warm_bytes, peak_bytes, calls_at_once } }
```

The aggregates repeat the structure `{ turns, answered, talked_over, bot_speaking, no_response,
v2v: { count, p50_us, p90_us, p95_us, p99_us, max_us } }`; `per_fixture` and `per_language` add `name`,
`per_turn` adds `fixture`, `index`, `lang`, `eos_us`. Raw per-turn samples are in `runs[]`, so any
other statistic can be recomputed.

Schema `v2v/2` added `engine` and renamed `sim` (the mock bot's settings) to
`mock_bot`; in `v2v/1` files sim mode always meant the mock bot. Schema `v2v/3` added
`p95_us`, the runs' `kind` and `engine_stages`.

## Capacity benchmark

`brisvox-bench capacity` ramps N concurrent calls against one websocket URL and finds the largest
N that keeps the p95 V2V within an SLO. Method, definitions, the numbers and the Linux
reproduction (`bench/capacity_linux.sh`) are in `docs/LATENCY.md`, "Capacity benchmark".
Each call is the V2V client of this document (`brisvox_bench::v2v::client::run_call`) playing one
fixture in real time; a step pools the answered turns of all its calls.

Options (`brisvox-bench capacity ...`):

| option | default | what |
|---|---|---|
| `--sim` or `--url URL` | | `--sim` starts `brisvox-bench engine-server` (the engine over `brisvox_sim` mocks) as a child process; `--url` measures any server (the Pipecat bot, a container) |
| `--pid PID`, `--pid-only` | | read CPU time and resident memory of this process and everything below it (`--pid-only`: the process alone) every `--sample-ms` (500) |
| `--label NAME`, `--note TEXT` | | names the system in the report; the note says how a `--url` server was started (printed in the file, since the benchmark cannot know) |
| `--floor-ms MS` | engine: computed | the V2V of an unloaded turn: the SLO is counted from it (required with `--url`) |
| `--slo-over-floor-ms MS` | 250 | allowed p95 V2V above the floor |
| `--max-bad-rate F` | 0 | share of the expected turns that may be lost or not clean (talked over, bot still speaking, no answer; failed calls count all their turns as lost) |
| `--steps LIST` | `1,5,10,20` | concurrent calls per step; the ramp stops at the first step that does not pass unless `--keep-going` |
| `--resolution N` | 0 (off) | bisect between the last pass and the first stop until they are N apart |
| `--rounds N` | 2 | calls each slot plays one after another in a step |
| `--warmup-calls N` | 1 | calls played alone before the ramp, not counted (a Python process, a cold cache) |
| `--spread-ms MS` | 4000 | slot i of N starts `spread x i / N` after the step began |
| `--min-gap-ms MS` | 4000 | silence between the fixture's utterances |
| `--cooldown-ms MS`, `--pause-ms MS` | 3000, 200 | between steps, between a slot's calls |
| `--client-lateness-ms MS`, `--client-late-share F` | 100, 0.01 | a frame sent more than this late is a *late frame*; a step in which more than this share of the frames was late, and which is otherwise healthy, is *generator limited* (neither pass nor fail: a lower bound). One outlier is not a verdict, and a step with failed calls, lost turns or a p95 over the SLO stays a fail (a stalled server also backs up the generator's writes) with the lateness named in its reasons |
| `--client-workers N` | 2 | tokio workers of the load generator |
| `--fixture NAME` | `en_two_utterances` | every call replays this fixture |
| `--out-md FILE`, `--out-json FILE` | stdout | written only if the run is valid (some turn was answered) |
| `--models DIR` | | `--sim` only (and `engine-server`): run the real Silero VAD and Smart Turn v3.2 (`DIR/smart-turn-v3.2-cpu.onnx`) instead of the energy VAD and the Smart Turn stub; needs a build with `--features models`; the report says `real models` |
| `--server-workers N`, `--max-calls N`, engine flags | | `--sim` only: the child's tokio workers, call limit (1024), and the engine target's flags of `v2v` (`--pool-threads`, `--pool-queue` (1024, the production queue), `--stt-final-ms`, ...), forwarded to the child |

JSON (`schema: "brisvox-bench/capacity/2"`; `/2` added `environment`, the late-frame counts and
`late_share_limit`, `engine.pool_queue`; all with defaults, so `/1` files still parse): `label`, `note`,
`url` (redacted), `environment` (`os`, `arch`, `logical_cores`, `release_build`, `not_linux_run`,
`banner`: a Windows trend says so in the file itself), `settings` (fixture,
steps, rounds, warmup_calls, spread_ms, floor_ms, slo_limit_ms, max_bad_rate, ...), `engine`
(the mock providers, `--sim` only), `warmup` (the warm-up step), `steps[]` and `conclusion`
(`max_concurrent`, `first_failure`, `limit`). A step has `concurrent`, `calls`, `calls_failed`,
`failures[]`, `turns_expected`, `turns_answered`, `talked_over`, `bot_speaking`, `no_response`,
`bad_rate`, `v2v` (`count`, `p50_us` ... `max_us`), `p95_over_floor_us`, `max_send_lateness_us`,
`late_frames`, `timed_frames`,
`wall_ms`, `rss_before_bytes`, `target` and `client` (`cpu_cores`, `rss_peak_bytes`, `processes`,
`readings`), `verdict` (`pass` | `fail` | `generator_limited`) and `reasons[]`.
