# Testing strategy

| Layer | What | Where |
|---|---|---|
| Unit | codecs, segmenter, state machine transitions, schema validation | each crate |
| Property | frame ordering, epoch dropping, tool-call assembly from random delta splits | core/agent (proptest) |
| Simulation | full sessions under paused time with mock providers and scripted users | brisvox-sim |
| Contract | every provider adapter vs shared trait tests + local protocol mock server | providers |
| Chaos | disconnects, stalls, slow consumers, panics across many sessions | brisvox-sim |
| Load | 10–500 concurrent sessions, overhead/CPU/memory (`brisvox-bench sim-load`; see LATENCY.md) | brisvox-bench |
| Live | real providers, `BRISVOX_LIVE_TESTS=1`, keys in .env | providers, bench |
| Benchmark | V2V vs Pipecat, capacity | brisvox-bench, bench/ |

Rules
- Every bug fixed gets a regression test (preferably a simulation scenario).
- Tests are deterministic: seeded RNG, injected Clock, no real network unless live.
- No sleeping in tests on wall time; use paused tokio time. Exception: the load harness's own
  smoke test (`brisvox-bench/tests/sim_load.rs`, ~8 s real time) — it asserts counts and leaks,
  never timing values. Timer-tick delays are invisible in paused time; guard them with
  deterministic counters (e.g. a counting `Clock`, `brisvox-sim/tests/pacer.rs`).
- Simulated time: build the runtime with `start_paused(true)` (or `#[tokio::test(start_paused =
  true)]`) and create `SimClock::new()` inside it. Timers then fire instantly in virtual time, in a
  reproducible order. Reference: `clock::tests::ten_minute_session_runs_fast_and_deterministically`
  (10 simulated minutes, 30 000 paced frames, two runs compared, < 1 s wall).
  Only current-thread runtimes can be paused; timer granularity is 1 ms; a `SimClock` created
  outside a paused runtime silently follows real time.
- Fixture provenance and licenses are recorded below.

## Fixture provenance
Real-speech fixtures live in `fixtures/` (built by `scripts/fixtures/build_fixtures.py`, loaded by
`brisvox_sim::fixture::Fixture`, played into a call with `SimSession::run_fixture`).

| Item | Value |
|---|---|
| Source | Google FLEURS, dev split, `en_us` and `tr_tr` (research and alternatives: `docs/FIXTURES_SOURCES.md`) |
| URL / pin | https://huggingface.co/datasets/google/fleurs, revision `70bb2e84b976b7e960aa89f1c648e09c59f894dd` (fetched 2026-10-07) |
| License | CC BY 4.0 (audio); transcripts from FLORES (CC BY-SA 4.0) -> `fixtures/` is shared under CC BY-SA 4.0, see `fixtures/NOTICE` |
| Access | anonymous HTTPS; the script reads only a prefix of each dev archive (en 16 MB, tr 64 MB), cached in `target/fixture-cache/` |
| Checksums | SHA-256 of every source clip in `fixtures/sources.lock` (small set) and `fixtures/large.sources.lock` (larger set), both tracked; a rebuild must select exactly the locked clips with the locked checksums or it fails (`--update-lock` re-pins deliberately); the build is byte-reproducible |
| Processing | float32 -> PCM16, peak-normalized to -6 dBFS, edge silence trimmed (10 ms energy gate), clips with internal pauses > 250 ms or < 1.5 s of speech skipped, concatenated with known silence gaps |
| Committed set | `en_two_utterances`, `tr_two_utterances`, `en_tr_mixed` (1.3 MB, 16 kHz mono) |
| Larger set | `--large` -> `fixtures/large/` (git-ignored), 4 fixtures x 3 utterances, pinned by `fixtures/large.sources.lock` |

Ground truth: each sidecar `fixtures/<name>.json` lists speech segments by sample (`start`,
`end`), language, transcript and source clip. One clip = one speech region (no internal pause
longer than the VAD hangover), so VAD start/stop can be checked exactly: the energy VAD matches
every committed fixture within 10 ms (test `fixtures::energy_vad_tracks_the_ground_truth...`).

Benchmarks: `brisvox-bench fixtures` plays all three committed fixtures, Turkish and
mixed included, as concurrent simulated calls and reports voice-to-voice per fixture and per
utterance language (`bench/results/fixtures-sim.md`, `docs/TURKISH.md` section 3). The gaps
between utterances are widened with digital silence so the caller does not talk over the answer.

Caveat: the segment boundaries are where the build script's 10 ms energy gate first/last fires
on the clip, not human-labelled speech onsets. For the energy VAD the check is therefore partly
circular (similar detector, same signal); it proves frame accuracy and hangover handling, not
that the boundary is where a listener hears speech end. Model VADs are measured against
the same boundaries, and differences of a few tens of ms at soft word edges are expected.

## Quality gate (`scripts/verify.sh`)
1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings` (workspace lints in the
   root `Cargo.toml`; test exemptions in `clippy.toml`)
3. `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`: an
   unresolved, private or redundant intra-doc link fails the gate. Link an item that is not in
   scope by its path (`[`TtsEvent::Word`](crate::TtsEvent::Word)`); do not repeat a path that
   resolves by itself.
4. tests: `cargo nextest run` + doc tests if nextest is installed, else `cargo test --workspace`
4. `cargo deny check` (licenses, advisories, bans, sources) if cargo-deny is installed and
   `deny.toml` exists

CI (`.github/workflows/ci.yml`) runs the same script on ubuntu-latest and windows-latest
with the pinned toolchain, `Swatinem/rust-cache`, and prebuilt cargo-nextest + cargo-deny
(`taiki-e/install-action`). nextest (`.config/nextest.toml`) runs each test in its own process
and kills a test that runs longer than 3 minutes, so a hang fails fast. CI also checks
`Cargo.lock` is current (`cargo fetch --locked`), runs weekly for new RUSTSEC advisories
(cargo-deny needs network access), pins third-party actions to commit SHAs, and saves the build
cache only from the default branch. First real run pending a git remote.

Lint exemptions apply only to `#[test]` fns and `#[cfg(test)]` modules. Files under `benches/`,
`examples/`, `build.rs`, and non-test helpers in `tests/` must carry `//!` docs and, if they use
`unwrap`/indexing, a file-level `#![allow(...)]`.

### Toolchain for `--all-features`
The gate builds with `--all-features`, which includes `brisvox-transport/webrtc`. That
feature compiles C code: libopus (`opusic-sys`, built by the `cmake` crate) and `aws-lc-sys`
(DTLS crypto of str0m, via `dimpl`/`rcgen`, also built with cmake). So the gate needs a **C
compiler and cmake** (Linux: `apt install cmake build-essential`; macOS: Xcode tools and
`brew install cmake`; Windows: MSVC build tools; the `cmake` crate finds the cmake bundled with
Visual Studio, otherwise install cmake). `scripts/verify.sh` checks for cmake first and says so.
The default build (`cargo build`, no features) and the `websocket`/`twilio` features need
neither. Not yet verified on a clean Linux box: whether `aws-lc-sys` additionally wants NASM
(it needs it on Windows x86_64 only when the prebuilt objects are unavailable; it did not
here). CI images (`ubuntu-latest`, `windows-latest`) ship cmake and a C compiler.

## Lessons (keep tests honest)
- Paused-time tests that wait for frames must wrap the wait in `tokio::time::timeout`: a missing
  frame otherwise hangs the whole test binary (a verify run timed out once).
- System-lane priority lets lifecycle frames overtake queued data; race tests must yield between
  data and lifecycle frames, or drive units directly.
- Mutation-check every regression/property test: re-introduce the bug, confirm the test fails.
- The gate runs on Linux and Windows (`.github/workflows/ci.yml`); a test that passed on one
  desktop only is a bug in the test. Reproduce Linux failures in a `rust:1.99.0` container
  (WSL2 + Docker on the Windows machine) with `cargo nextest run --workspace --all-features`; for
  racy ones add `--stress-count N` and CPU contention (`docker run --cpuset-cpus=0,1`, busy loops).
  The causes found: model goldens that are platform-specific (Smart Turn: only compared where the
  reference's kernels run, `docs/providers/smart-turn.md`; Silero: 1e-7 on Windows, 5e-6 elsewhere
  with a proof that no golden window is within 10x of a threshold); kernel socket buffers (Linux
  autotunes loopback buffers to MiB, Windows holds ~0.5 MiB: bound by the platform's limit, not a
  fixed count); tokio's cooperative budget and a current-thread runtime that only polls its IO
  driver when the task yields (a test that expects a message to be seen between two always-ready
  sends must let the runtime run); the scheduler's lazily allocated defer list (one allocation the
  first time a task yields: warm it before counting); work started "together" that is only
  ordered by thread start-up (wait for the first job to run before filling the queue behind it).
- Tests that fail only on a busy runner (reproduce with `docker run --cpuset-cpus=0,1` plus
  busy loops, and `--stress-count`): (1) "a client that does not read" tests with real sockets set
  a tiny `SO_RCVBUF` on the client they own (`TcpSocket::set_recv_buffer_size` before `connect`,
  see `websocket.rs`, `rtvi.rs`): the kernel's autotuned buffers grow and drain while the test
  runs, which let one more reply through (17 for 16 slots) only under load; where the client is
  the adapter under test (providers) the script is sized from the machine's buffer maxima
  (`brisvox_sim::platform::loopback_buffer_bytes`) instead. A test that blocks a writer waits
  until the writer made no progress, not until a queue happened to fill. (2) Real-time CAPACITY
  tests (`realtime_load`, `early_analysis_realtime`'s latency test) assert how the machine copes,
  not what the engine does: they keep their strict assertions, run alone (nextest
  `threads-required = "num-test-threads"`) and are retried twice in the `ci` profile
  (`.config/nextest.toml`, `scripts/verify.sh` uses it when `CI=true`); a retried pass shows as
  FLAKY in the log. Correctness assertions are never retried: add a test to that filterset only
  after showing that it fails on a loaded machine and that the engine's behaviour was right.
  (3) Statistics of a model on synthetic inputs (the early-analysis cut sweep) belong to the
  platform's kernels like the goldens do: strict where the reference runs, the safe direction
  pinned everywhere.
  (4) A raw-protocol test peer behaves like the real peer or the test is racy: the SIP `Carrier`
  of `hosting.rs` acknowledged the 200 once, and `rsipstack` can miss an ACK that arrives within
  100 us of its 200 (13 of 600 runs under CPU load, then a 10 s wait); a real UAC re-ACKs every
  retransmitted 200, so the carrier repeats its ACK. And a test that reads a mock's record right
  after the adapter's stream ended must wait for the last message (`eventually`).

## Session simulation
- `brisvox_sim::scenario`: a `Scenario` (`speak(text, duration)`, `silence(duration)`) is the
  caller's script. `play_input` sends one 20 ms PCM16 frame per frame period on the paused clock
  (speech = square wave, silence = zeros) into a pipeline; `speech_segments()` is the ground
  truth; `utterances()` scripts `MockStt`; end scripts with `with_tail(>= hangover)` or the
  last utterance never stops. `play_input` takes a `CancellationToken`. `SimTransportOut` is
  the epoch-strict output transport: it records every frame it gets (time, kind, epoch, audio
  length; bounded by `with_record_limit`, default 100k) and marks `FirstAudioOut`, and on an
  interrupt `AudioOutStopped` for the turn it was playing.
- `brisvox_sim::vad::EnergyVad`: RMS threshold VAD (start after 40 ms of speech, stop after
  300 ms hangover by default), timestamps backdated to the first speech frame / end of the last.
- The user-input side (transport in, VAD, STT, turn manager) is epoch-agnostic (`KeepAll`):
  barge-in audio must never be purged (guarded by a test). Epochs begin at the turn manager,
  which also records the VAD/STT/turn timeline marks.
- `brisvox_sim::reference` + `brisvox_sim::session::SimSession`: a whole call. Reference
  stages (a stand-in for the production engine): `SttStage` (Finalize after
  forwarding the VAD stop; `UtteranceFinal` control on speech_final or session failure;
  reconnect with backoff), `TurnManager` (epochs, turn end once the VAD stopped and every
  finalize is answered, barge-in when the bot is active -> `Interrupt`, timeline marks),
  `LlmStage` (history; after a barge-in the assistant message is cut to the played words),
  `TtsStage` (context = epoch, priority cancel, word log, `TtsEnded` even on failure),
  `OutputPacer` (real-time playout; waits abandoned as soon as an interrupt is queued; played
  audio log; `TtsEnded` upstream when a turn's audio is out). `SimSession::run(scenario,
  linger)` returns the transport records, finished turn timelines and probes (history, words,
  played). Use a fresh `SimSession` per run (mocks are shared state).
- First finding of the simulator: a pacer sleeping through a 40 ms chunk delayed barge-in by up
  to a chunk; fixed with `ProcessorContext::epoch_watch`.
- `brisvox_sim::assertions` (on `SessionRun`): `check_bot_audio_within(after, max)`,
  `check_no_stale_audio_after_interrupt()`, `check_context_matches_played_text()` (every
  assistant message equals the words that started playing in its turn), `played_text(epoch)`,
  `audio_of(epoch)`, `first_bot_audio_after(t)`, and `fingerprint()` for determinism. The
  scenario suite (`crates/brisvox-sim/tests/scenarios.rs`, 11 calls: happy path with realistic
  seeded latency, barge-in, double utterance, long silence, provider stall, LLM error, STT
  disconnect, tool call with text, back-to-back turns, interrupt during TTS first-byte wait, TTS
  disconnect mid-answer) runs every scenario twice and requires identical fingerprints plus the
  stale-audio and played-text invariants. New bugs found while building it are fixed in the
  reference stages and keep their scenario as a regression test.

## Chaos
`crates/brisvox-sim/tests/chaos.rs` runs 100 simulated calls concurrently in one paused-time
runtime. Every third call is unaffected; the rest get one seeded fault each: STT disconnect or
stall, LLM 503 / disconnect mid-answer / 3 s stall, TTS disconnect or stall, a mid-turn barge-in,
a slow transport consumer (`SimSession::transport_delay`, 30 ms per 20 ms frame), or a hang-up
(the call future is dropped mid-answer). A warm-up round runs first and its tasks must wind down
before the baselines are taken. Checks:
- no call panics or fails a stage (`outcome == Ok`); every round is bounded in virtual time;
- every call keeps the invariants: no stale audio after an interrupt, the LLM context holds only
  played text, no transport records dropped;
- every fault fired: disconnects show up as reconnects (`MockStt/MockTts::connects`; the stages
  may hide them completely), every other fault changes the call versus the same call without it;
  barge-ins interrupt; the slow consumer really spaces frames >= 30 ms;
- recovery: whatever failed in the first turn, the second question is answered;
- isolation: each unaffected call equals the same call run alone (fingerprint: output timing,
  history, words, metrics) and answers each turn within 1.5 s of its end of speech;
- no tasks outlive the round (hung-up calls included; 100 ms virtual grace) and live heap bytes
  return to the pre-round level within 4 KiB (a counting `GlobalAlloc` in this test binary only).
  Measured: +556 B after 100 calls.
Writing these checks found two faults that had silently been no-ops (a disconnect after the
transcript was complete; a single-delta LLM answer). Limitation: paused time cannot show blocking
or CPU-heavy work on the runtime; real-time behaviour under load is `brisvox-bench sim-load`
.

## Reusable suite and the sim seam
The scenario, played-text, fixture and chaos bodies live in `brisvox_sim::suite` as
`async fn name<F: SessionFactory>(factory: &F, profile: Profile) -> brisvox_sim::Result<()>`
(`tests/scenarios.rs`, `played_text.rs`, `fixtures.rs`, `chaos.rs`, `session.rs` are thin wrappers
over `ReferenceFactory` + `Profile::Exact`). `SessionFactory::build(SessionParts)` returns a
`BuiltSession { pipeline, probes, input: InputSink, output: OutputLog }`; `SimSession::run_with`
builds, starts, binds the input, plays the caller and collects a `SessionRun`.
- `Profile::Exact` asserts the reference's closed-form timings (1480 ms first audio, 660 ms of
  audio, 280 ms end-of-turn); `Profile::Budget` asserts ranges (table in the `suite` module docs).
  Structural invariants (outcome, determinism, no stale audio, context == played text, roles, turn
  and interrupt counts, recovery, isolation, no leaked tasks, flat heap) are identical in both.
  Production fingerprints are compared only to production fingerprints.
- Interrupt counts come from `Interrupt` frames recorded at an output tap (`OutputLog::tap`), never
  from client clears. `OutputLog` also holds wire-arrival records (`record_wire`) for pipelines
  with a transport; `LoopbackConfig::mark_acks` (`Manual`, `Auto { delay }`, `Never`) lets the
  loopback client echo marks for them.
- The suite can fail (`tests/faulty.rs`): a wrapping factory injects one fault into the reference
  chain and the test pins which profile rejects it. Measured, not assumed: +100 ms end-of-turn
  silence (first audio 1580 ms) and +40 ms barge-in confirmation (interrupt at 2460 ms) fail Exact
  and pass Budget (Budget is tight: +20 ms, interrupt at 2440 ms); a 30 ms per-frame
  transport delay fails Exact's `time to silence` and passes
  Budget; 150 ms per frame (260 ms to silence), a dropped `Interrupt` at the output tap and old-turn
  audio after the interrupt fail both; an LLM that is slow to its first token and keeps one unheard
  word of the answer when the caller barges in before any audio fails the streaming check in
  both. Each was mutation-checked (suite body switched to `Profile::Budget`, Budget range widened,
  `check_no_stale_audio...` removed, `interrupts == 1` weakened: the matching test fails).
  Nothing-heard-means-nothing-kept has no word of slack in either profile (the Budget word slack
  covers a word at the cut only); a Budget pipeline that drops a barge-in's response whole leaves
  the reply to the second utterance as its first assistant message, which the check treats as empty.
- Loopback mark echoes (`MarkAcks::Auto`): the transport tests pin that a zero-delay echo reaches
  the engine at the instant the client received its mark even with audio due later behind it
  (the adapter must not serve its delivery queue first), that a hand-sent echo under `Auto` is
  delivered at once and counted once, and that the auto echo still fires for other marks. The
  tie-break when an echo and a delivery fall due at the same instant is pinned only by the unit
  test of `next_wake`: the two go to different channels, so no timestamp can show the order.
- Refactor proof: before the shared-suite refactor every fingerprint and output record of every sim test (1709
  dumped lines, tests run single-threaded) was captured and compared byte for byte after the
  refactor (identical). The capture hook was temporary and is not in the tree.

## Hybrid gate 3 accommodations
`crates/brisvox-engine/tests/hybrid_llm.rs` runs the shared suite with the engine's input chain,
`SttStage`, `TurnDriver` and `LlmRunner` in front of the reference TTS/pacer/output. A gate-3 pass
is conditional on three accommodations, and the production pipeline (and any later gate) must not inherit them
silently:
- `llm.retries = 0`. The suite's s06 scripts one failing response followed by the NEXT turn's
  answer; with the production default (one retry) the retry would consume that answer. So a pass of
  `all_scenarios_*` does not prove s06 under the default configuration. The retry itself is tested
  in `tests/llm_runner.rs`, and `every_scenario_but_s06_passes_with_the_default_retry_configuration`
  runs s01..s05 and s07..s11 with `retries = 1`.
- The played-text suite runs with a 300 ms fixed silence ("Quick") instead of the 450 ms used for
  s01..s11: its barge-ins come 0.4 s after the first utterance.
- `InterruptionConfig::OFF` for the basic suite (its barge-in budget is that of an immediate
  barge-in; the guards deliberately delay it).

The runner's failure paths (a stuck round task, a truncation source that never answers, a refused
offload, a panicking provider, a request that cannot be built, Cancel, the record bound, oversized
`provider_data`, a summarizer that is slow or never answers) are tested in `tests/llm_runner.rs`
and `tests/llm_window.rs`. The stuck-task test blocks a real worker thread, so it runs on a
multi-thread runtime in real time and waits for the blocked request without the runtime's timers
(while a worker is blocked and the others sleep, nothing drives the timers).

## The shared suite on the production pipeline
`crates/brisvox-engine/tests/engine_suite.rs` runs `brisvox_sim::suite` under `Profile::Budget`
on `brisvox_engine::sim::EngineFactory` (every stage of `build_session_pipeline` over a loopback
`TransportSession`, paused `SimClock`): s01..s11 with fixed 450 ms silence and the recommended
guards, with every guard off, with the adaptive policy (fixed-answer model) and with the `time_in`
tool; s08 with the registered tool (`s08_with_a_registered_tool`: call, the tool's own result,
second round, both spoken); every scenario but s06 with the default retry; the basic suite (fixed
and adaptive); the played-text suite; the three fixtures (en, tr, mixed; first audio at most
600 ms after each utterance end, measured 280 to 475 ms). Every suite function runs each call
twice and requires identical fingerprints (production against production). `run_call.rs` covers
the library entry point (stop, hang-up, cancel ends the pump too, no task left behind, clear hook,
early-analysis switch, warm-up of every stage); `warm_pool.rs` the per-thread model warm-up on a
real pool. The Exact profile runs on the reference only.

### Divergences of the production pipeline from the reference
Each is either a property of the production design or an accommodation of the suite's scripts;
none is silent:
1. **Output through a real transport.** Audio goes through `TransportOut`, the session's pacer and
   the loopback client: a `Clear` drops queued audio, end marks are echoed (auto, no delay),
   positions are reported per 20 ms frame. Until the phase was turned off the pacer had a per-session phase offset
   (0 to 20 ms) on the session's first frame, so first audio came up to one frame after the
   reference's (measured +15 ms: 1495 ms with the adaptive policy against 1480 ms); now the first
   frame leaves at once, like the reference's (the offset is an opt-in, `with_first_frame_phase`).
2. **Output log on wire time.** The tap in front of `TransportOut` records every frame but audio as
   it passes (so `Interrupt` counts are the pipeline's, not the client's clears); audio is
   recorded when the client received it (`OutputLog::record_audio`). A tap before the transport
   would see audio up to a second early (the pacer queue), which the played-text oracle cannot
   use. Audio received at the same instant as an `Interrupt` is filed before it in the output
   log (the tap stamps the interrupt with the same paused-clock time), so the output-log stale
   check only sees stale audio a tick or more later; `check_no_stale_audio_after_interrupt` also
   walks the wire records in the client's arrival order (no audio of turn `e` after a `Clear` to
   a later turn), which catches a stale frame right behind the clear in the same instant
   (`tests/faulty.rs`, `stale_audio_right_behind_the_clear_on_the_wire_fails_both_profiles`,
   mutation-checked).
3. **Context vs heard text: up to one word fewer, never more (Budget only).** The book counts a
   released frame whole and subtracts `lead_margin` (one 20 ms frame in the factory); the oracle
   plays a frame continuously from its arrival. The engine's heard position is therefore 0 to
   20 ms behind the oracle's, and a word that started in that window is left out of the context.
   Measured: s02 with the adaptive policy kept "...there was a voice" while "agent" had started
   (it passes with a lead margin of 0 to 15 ms). `Profile::context_holds_played` applies the
   `BUDGET_WORD_SLACK` of 1 word only to turns a barge-in ended (an `Interrupt` to the next
   epoch reached the output, `SessionRun::cut_by_barge_in`): such a turn's context may lack one
   heard word, or be left out if at most one word was heard. A turn that played to its end must
   match exactly, and the context never holds more than was heard. Under Exact (the reference)
   the check stays exact. `engine_suite.rs`
   (`with_no_lead_margin_the_context_is_exactly_the_heard_prefix_after_every_barge_in`) runs
   s02, s10 and the basic barge-in with `lead_margin = 0` and the exact check, so a regression
   that loses the last heard word at every barge-in cannot hide behind the slack (mutation:
   a 40 ms margin fails it).
4. **Multi-round turns.** A tool turn has two assistant messages in one epoch; the context check
   joins the assistant texts between two user messages (`SessionRun::said_per_turn`). The probe
   history now carries the assistant's tool calls (`provider_messages` maps them).
5. **s08 runs the tool loop.** Budget accepts the reference's shape (tool ignored) or the full
   loop (call on the first round's message, its result, the second round's scripted text, both
   spoken); `s08_with_a_registered_tool` requires the loop and the tool's own result (an
   unregistered tool's `unknown_tool` error result fails it: mutation-checked).
6. **`llm.retries = 0`** in the factory (inherited gate-3 accommodation): s06 scripts the next
   turn's answer after the failure. `every_scenario_but_s06_passes_with_the_default_retry_
   configuration` covers the rest with the default (one retry).
7. **Played-text suite at 300 ms fixed silence** (inherited): its barge-ins come 0.4 s after the
   first utterance; with 450 ms the caller would resume the first turn.
8. **Basic suite with every guard off** (inherited): its barge-in budget is an immediate
   barge-in; the recommended guards wait for words (measured: the interrupt at 3 s instead of
   2.42 s). s01..s11 run with the guards.
9. **Turn end.** Fixed 450 ms silence (end-of-turn delay 450 ms against the reference's 280 ms,
   first audio 1665 ms) or the adaptive policy with a fixed model (280 ms, as the reference).
   `vad_hangover` is set from the simulator's VAD (`stop_after`), since its stop is backdated.
10. **Rates.** `SessionParts::input_rate`/`output_rate` are not used: the loopback call runs at the
    engine's fallback rates (16 kHz both ways); the mock TTS delivers 24 kHz and `TtsStage`
    resamples. `SessionParts::transport_delay` (slow consumer) is a slow reader of the loopback
    client (one-event client queue, the delay after each audio frame, frames
    stamped when read), so the chaos suite's 30 ms minimum frame gap holds for both pipelines.
11. **VAD.** The simulator's energy threshold runs as an `EnergyModel` behind the production VAD
    worker (batching, gap handling, `Offload::Inline`); speech marks measured equal to the
    reference's (200 ms and 1200 ms).
12. **Played-text streaming check.** Under Budget "everything played" means every word the TTS
    spoke for the turn had started at the barge-in (a production segmenter holds the last
    sentence of a stalled stream back, so less than the delivered text may be spoken).

The Budget ranges were tightened from measurements (`docs/LATENCY.md`, "Simulator Budget
numbers"); `tests/faulty.rs` follows them (a 20 ms slower barge-in confirmation passes Budget, a
40 ms one fails both profiles on "interrupt at the output", pinned by
`a_40_ms_slower_barge_in_fails_both_profiles`; a 150 ms-per-frame transport now fails one turn's first audio in both
profiles; a 30 ms one passes Budget's first audio and time to silence).

## Chaos, isolation, leak, allocation and blocking on the production pipeline
- **Chaos** (`crates/brisvox-engine/tests/engine_chaos.rs`): the shared 100-call chaos suite
  (`brisvox_sim::suite::chaos`) on `EngineFactory`, Budget profile, its own counting allocator.
  Every check of the reference run holds: outcome Ok, no stale audio (tap and wire), context ==
  played (the exact heard prefix after a TTS disconnect; 6 calls cut mid-answer with text heard),
  every fault had an effect, recovery on the next turn, the 34 unaffected calls identical to the
  same call alone and answering within the 1.5 s SLO, no task left behind, heap +604 B after the
  round (limit 4 KiB). About 35-40 s in a debug build (nextest override: 120 s slow period).
  The slow consumer is a slow reader of the loopback client (`LoopbackClient::split`): a
  one-event client queue, `transport_delay` after each audio frame, frames stamped when read;
  the suite's minimum frame gap is now 30 ms for both pipelines (mutation: ignoring the delay
  fails "fault had no effect"). Accommodations: the adaptive end of turn with a fixed model
  (280 ms, as the reference; for the shared SLO set by the reference); a factory that keeps no
  handles (`without_handle_log`: 166 calls' books and contexts are 2 MB the heap check would
  count). The first TTS chunk is the production default (one word): the
  `with_first_chunk_words(1)` override is gone (with the old default of three, the 10-word first
  sentence the chaos LLM streams word by word, 30 ms apart, was spoken only when complete, and a
  call drawing a slow first token answered in 1.59 s, seed 2009; `first_chunk_policy.rs` below).
- **Allocation per frame** (counting allocators, thread-local, current-thread runtimes):
  `stt_turn_alloc.rs` 0 allocations over 200 frames of steady speech and 200 of steady silence
  through AudioIn (VAD tee and worker) -> SttStage (into the provider channel) -> TurnDriver,
  with and without the Smart Turn ring. The VAD worker runs on the **inline** offload there. On
  the inference pool (production) every batch job costs 3 allocations on the submitting tokio
  thread (the reply channel, the abandonment flag, the boxed job; pinned exactly by
  `a_pool_job_costs_a_fixed_number_of_allocations_on_the_submitting_thread`), and the worker
  forms about one batch per 20 ms frame, so the production AudioIn -> VAD path costs about 3
  allocations per frame (bounded, not zero). The frames carry shared buffers, as the transports make
  them: a `Bytes` made from a `Vec` allocates its shared header at its first clone, which is the
  STT stage's (one allocation per frame, measured, the buffer's not the engine's).
  `audio_in_alloc.rs` (0 per frame, every rate) and `tts_alloc.rs` (per provider chunk, none
  per frame) as before. Per-chunk and per-word costs: TTS two allocations per provider chunk and
  one `Box` per word frame; segmenter at most 4 per emitted chunk plus 3 per push with sentence
  ends (`segmenter_alloc.rs`); an LLM text delta appends to the assembler's `String`
  (amortised doubling). `request_alloc.rs`: a request build allocates the same (1427
  allocations, 1.19 MB, the galloping clip of 12 results) for 12 x 20 KB and 12 x 60 KB results;
  a copy of the results before clipping (mutation: `to_string` of every message) adds 480 KB and
  fails. It replaces the wall-clock ratio of `llm_window.rs` as the deterministic guard; that
  test now takes the best of 15 interleaved runs.
- **Real time under load** (`realtime_load.rs`, feature `silero-tract`): 20 calls at once over
  loopback transports, multi-thread runtime (4 workers), system clock, a real pool (8 threads),
  Silero on tract, a Smart Turn model that sleeps 15 ms per analysis, every LLM finish forced
  onto the pool (`inline_finish_bytes = 1`). Every VAD window, analysis and finish ran on a pool
  thread (thread names), none on a worker; every call answered both turns; no pool rejection.
  Scheduler-gap probe (one yielding task per worker, longest time between two of its polls):
  debug 43-55 ms worst (bound 250 ms: a shared machine, unoptimised code), release 1.67, 1.84,
  2.27 and 2.61 ms worst over four runs with at most two polls over 2 ms per 14 s run, on the
  shared machine with other builds running. The strict 2 ms bound is asserted by the ignored
  release test only with `BRISVOX_QUIET_MACHINE=1`; otherwise it allows three gaps over 2 ms and
  none over 5 ms. **Open:** two of the four runs miss the 2 ms acceptance bound, and the quiet
  machine run has not been recorded yet. Single gaps look like OS preemption (a blocking stage
  would repeat every frame), but the probe does not record which stage ran during a gap, so this
  is not proven. Pool in release: wait max 18-36 ms, run max 18-20 ms (the 15 ms model).
- **Pool saturation and timeouts in a live call** (`warm_pool.rs`,
  `a_saturated_pool_degrades_the_call_but_leaves_it_alive`): a one-thread pool kept busy with a
  full queue during the caller's speech; the VAD counts refusals or timeouts, every frame still
  reaches STT, the VAD recovers after the pool frees up, the call stops cleanly. The runner's and
  the request builder's offload failures: `llm_window.rs`
  (`a_timed_out_build_falls_back_off_the_worker_even_while_the_context_lock_is_held`, a
  ticker on a current-thread runtime; the old inline fallback stalls it and fails).
- **Warm-up** (`warm_pool.rs`): a busy pool is not warmed and both claims cool down; a warm job
  that outlives its timeout keeps the claim until it ends (mutation: jobs without a share of the
  claim fail it); the Smart Turn warm-up fans out over half the pool (four jobs side by side on
  eight threads); VAD claims are per model id. `brisvox-inference`: the claim cool-down
  (`pool.rs`), concurrent `SmartTurnOrt::warm` calls create the target in all (`smart_turn.rs`).
- **Stats** (`run_call.rs`, `the_calls_counters_are_visible_through_stats_and_tracing`):
  `Call::stats().stages` and the `call stats` event on `brisvox::engine`, captured by a minimal
  tracing subscriber in the test.
- **Carry-overs**: the stale end mark after a `Clear` (`transport.rs`, `transport_out.rs`; the
  latter fails with the plain `send`), the margin fade up to the clear (`playout.rs`, two
  stepped-clock tests including a context round followed by a cut filler; both fail without the
  clear stamp).
- **Micro-benches** (`hot_path_cost.rs`): numbers in `docs/LATENCY.md`, "Hot-path micro-benches".

## How the production pipeline is proven (summary)
The production pipeline (`brisvox-engine`) was built against the untouched reference pipeline of
`brisvox-sim` in four hybrid gates, each a group of engine stages in front of the reference
stages, then as a whole (`docs/PIPELINE.md` section 9). Every gate runs the shared suite
(`brisvox_sim::suite`) on a paused clock through a real `TransportSession` over the loopback
transport.

| Profile | Asserts | Runs on |
|---|---|---|
| `Profile::Exact` | the reference's closed-form timings (1480 ms first audio, 280 ms end of turn, 660 ms of audio) plus every structural invariant | the reference only |
| `Profile::Budget` | ranges (table in the `suite` module docs, set from measurements: `docs/LATENCY.md`, "Simulator Budget numbers") plus the same structural invariants | every hybrid gate and the production pipeline |

Structural invariants are identical in both: outcome Ok, determinism (a call twice gives one
fingerprint), no stale audio after an interrupt, context == played text, roles, turn and
interrupt counts, recovery, isolation, no leaked tasks, a flat heap. Production fingerprints are
compared only to production fingerprints.

| Gate | Engine stages | Reference stages | Test | Accommodations |
|---|---|---|---|---|
| 1 | pump, `AudioIn`, VAD worker | STT, turn manager, LLM, TTS, pacer, output | `hybrid_input.rs` | none |
| 2 | + `SttStage`, `TurnDriver` | LLM, TTS, pacer, output | `hybrid_turn.rs` | fixed silence and adaptive (mock model), with and without guards |
| 3 | + `LlmRunner` | TTS, pacer, output | `hybrid_llm.rs` | see "Hybrid gate 3 accommodations" |
| 4 | + `SegmenterStage`, `TtsStage`, `TransportOut` | the session harness only | `hybrid_output.rs` | none |
| all | `build_session_pipeline` / `run_call` (`EngineFactory`) | none | `engine_suite.rs`, `engine_chaos.rs`, `run_call.rs` | see "Divergences of the production pipeline from the reference" |

Pre-warming has its own checks: `hybrid_prewarm.rs` (every warm-up and connect finishes
before the first VAD stop, failing and hanging warm-ups), `warm_pool.rs` (model warm-up on every
pool thread) and, on the production assembly, `prewarm_first_turn.rs`: with an STT
connect of 300 ms, a TTS connect of 2.5 s and an LLM `warm()` of 1 s, a caller who speaks after
3 s gets the first answer exactly as fast as the second (within the 20 ms frame grid), one
connect per session for the call; with a 0.5 s lead the same check sees the connect on the first
turn (about 390 ms slower), so it can fail.

`first_chunk_policy.rs` plays the chaos suite's 100 unaffected reference calls alone
(`brisvox_sim::suite::chaos::unaffected_first_audio`) under the production first-chunk default and
under three words, prints the first-audio distribution and asserts that the default keeps every
turn within the 1.5 s SLO. With three words 9 of 200 turns are over it, so the test fails
on the old default (checked).

The engine's per-stage times and memory per call are measured in real time by
`brisvox-bench v2v --sim` (`docs/LATENCY.md`, "Engine stage times and memory");
`crates/brisvox-bench/tests/v2v.rs` checks that every latency turn gives each stage, that the
barge-in calls give the clear, and that the memory calls run at once and clean.

## Provider contract suite
`crates/brisvox-sim/src/contract.rs` holds generic checks that every STT, LLM and TTS adapter
must pass: `stt_contract`, `llm_contract` and `tts_contract` return a `ContractReport` (one
`CheckResult` per check, `passed()`, `failures()`). The suite talks to a provider only through
the provider traits and a harness trait per kind (`SttHarness`, `LlmHarness`, `TtsHarness`) that
scripts the behaviour and injects faults. The sim mocks implement the harnesses directly;
`crates/brisvox-sim/tests/contract.rs` runs all three suites against them (with a 2 s deadline
from `CONTRACT_TIMEOUT`) and against broken doubles.

**Bounds: a hang is a reported failure.** Every provider call (`connect`, `stream`, `warm`) and
every awaited event is bounded on the injected clock by `3 x io_timeout`; each check as a whole is
bounded by `CHECK_BOUND` (60 s). A call that never returns fails the check that made it, and the
failure names the call. Nothing waits on a real clock, so the suite runs in paused time under
simulation.

Checks:
- **STT**: `streaming` (the first event is the interim before any `Finalize`, then the final with
  `from_finalize` and `speech_final`); `cancel` (after the hang-up the events close within
  `CANCEL_PROMPT` = 500 ms and nothing follows; transcripts already handed over before the hang-up
  are read and discarded after a 50 ms settle, because the caller cannot tell them from in-flight
  ones); `reconnect` (a mid-session drop surfaces as `Disconnected`, the stream then ends, and a
  new session works); `error_mapping` (refusals 401/429/500/503 map to `Rejected { status }`, a
  protocol violation to `Protocol`, a drop to `Disconnected`); `timeout` (see "Timeout checks"
  below); `warm` (`warm()` succeeds, twice, and the next connect is not slower than a cold
  one plus 50 ms); `no_task_leak` (tasks return to the baseline after a finished and after an
  abandoned session, the latter dropped while the provider is mid-way through a long response).
- **LLM**: `streaming` (text deltas join to the text, tool-call fragments join to the JSON, the
  first fragment carries id and name, `Done` is last with `ToolCalls`, then the stream ends);
  `cancel` (first a control run proves that `LlmHarness::events_sent` is live: a response of three
  deltas plus `Done` must count at least 4; then the response must still be streaming at the drop,
  the counter must have advanced by at least the two deltas read, and at most `CANCEL_SLACK_EVENTS`
  = 2 more response events may be produced within `CANCEL_PROMPT` after it); `reconnect`,
  `error_mapping`, `timeout`, `warm`, `no_task_leak` as above. The `no_task_leak` hang-up scripts
  the long `CANCEL_RESPONSE_EVENTS` response, so an adapter that keeps pulling the response
  after the drop outlives `LEAK_BOUND` and is seen there as well as in `cancel`.
- **TTS**: `audio_and_words` (PCM16 audio and words in order, word offsets non-decreasing and the
  audio lasts until the last word starts; the first word need not be at offset 0, because
  providers report alignment after leading silence; nothing for the context after its `Flushed` within a 500 ms
  quiet window); `flush` (words continue across text pieces, offsets strictly increase, the audio
  covers the words, nothing after `Flushed`); `cancel` (the first audio is read, then, after a 50 ms
  settle, whatever is buffered in the event channel is read and not counted, because output
  already handed to the caller is not the provider's to take back; then the cancel is sent and the
  priority cancel channel gets a `Cancelled` ack within `CANCEL_PROMPT`, at most
  `CANCEL_SLACK_EVENTS` = 2 events of the cancelled context arrive after the cancel, nothing
  follows the ack within the quiet window, and the session still serves the next context); `reconnect`, `error_mapping`, `timeout`, `warm`, `no_task_leak` as above.
- **Timeout checks** (all three kinds; a stall is `Fault::Stall` / `Fault::ConnectStall`, "slow"
  is half the deadline, 1 s for the 2 s `CONTRACT_TIMEOUT`):
  1. a connect stalled for an hour yields `Core(Timeout)`;
  2. a slow connect inside the deadline must succeed (LLM: and its response arrive whole);
  3. a first event stalled for an hour yields `Core(Timeout)`;
  4. a first event that is slow inside the deadline must be delivered (TTS: the whole utterance);
  5. an idle gap after the first event (`before_event: 1`, an hour long) yields `Core(Timeout)`,
     so a deadline armed only for the first byte is caught.
  Every `Timeout` is judged both ways: not later than the deadline plus `TIMEOUT_SLACK` (100 ms)
  and not earlier than the deadline minus `TIMEOUT_SLACK`. An adapter with a hard-coded shorter
  deadline fails 2 or 4 (and the lower bound), one that fails fast fails the lower bound. The
  timer starts when the stall begins: after the connect call starts, after the input that
  triggers the event is sent, or after the previous event was read.
- **Task leaks**: the baseline is taken once the runtime's alive-task count has been stable for
  `LEAK_BOUND` (2 s), so tasks of earlier checks that are still winding down are not counted
  against the check. Limitation: the count is runtime-wide, so a task of another check that exits
  inside the window can offset one leaked task there. Tracking the session's own tasks needs a
  per-adapter hook and is not done.

- **Harness assumption: one frame is one event.** Timeout check 5 ("idle gap after the first
  event", `Fault::Stall { before_event: 1 }`) stalls the second frame the harness mock sends, so a
  frame must map to one event. A service that puts audio and alignment (words) into one message
  (ElevenLabs) makes a frame two events (Audio, Word); its harness mock therefore sends the
  alignment of a word as its own message ahead of its audio. The suite then does not run the
  real message shape through the cancel, timeout and reconnect checks; that adapter's own
  protocol tests (`tests/elevenlabs.rs`) cover those behaviours with audio and alignment in one
  message.

- **Providers without word times.** `TtsHarness::emits_words()` (default `true`) lets a TTS
  adapter whose service has no timestamps at all (Deepgram Aura) say so. The suite then checks
  that audio arrives, `Flushed` closes the context and no `Word` event is made up, and skips the
  comparison of words with the audio (`audio_and_words`, `flush`); every other check is unchanged.
  `tests/deepgram_aura.rs` runs the suite twice, with `FlushPolicy::PerText` and with the default
  `PerTurn`, and both must pass in full. Under `PerTurn` a bare text owes nothing, so the adapter
  probes the connection with a WebSocket `Ping` while unflushed text is on the server: a silent
  server is a `Timeout` at the deadline (the `timeout` check stalls after a bare text), a live one
  answers and a slow LLM is not cut off (`tests/deepgram_aura.rs`, the probe tests).
  `Broken::DeclaresNoWords` in `tests/contract.rs` (a double that emits words but declares none)
  proves that the `emits_words() == false` branches can fail.

- **Request-scoped providers.** `TtsHarness::faults_at_connect()` (default `true`) lets a TTS
  adapter that opens nothing at `connect` (one HTTP request per text piece, the OpenAI-compatible
  speech adapter) say so. `Fault::Reject` and `Fault::ConnectStall` then apply to the first
  request of the session, and `error_mapping` / `timeout` demand the same errors as events after
  the first text (`Rejected { status }`, `Core(Timeout)` within the deadline) instead of from
  `connect`; the other faults and every other check are unchanged. `tests/openai_speech.rs` runs
  the suite against a mock HTTP/1.1 server on its own thread (chunked PCM, one chunk per word) and
  adds protocol tests: streaming before the body ends, joined pieces, connection reuse, cancel
  (in flight, before the headers, queued text), odd-sized chunks, WAV headers split byte by byte,
  refused pieces, cut bodies, idle timeout, dropped session and shutdown.

Each check must be able to fail, and `tests/contract.rs` checks that it does. The broken doubles
and the failure each must produce:
- `Leaky` (ignores the hang-up): STT `cancel` and `no_task_leak`; LLM `cancel` (generation goes on
  after the drop) and `no_task_leak` (the task outlives the long response's hang-up); TTS
  `no_task_leak`.
- `MisMapped` (refusals lose their status, drops become protocol errors): `error_mapping`.
- `WarmFails`, `WarmHangs` (`warm()` errors or never returns): exactly `warm`.
- `WarmSlows` (the connect after `warm()` takes 200 ms longer): exactly `warm`.
- `ConnectHangs` (`connect`/`stream` never return): every check still reports, and `timeout` and
  `warm` fail.
- `StaleSession` (after a drop, the next connect hands out a closed session): `reconnect`.
- `NoInterims` (STT): `streaming`. `EmitsAfterHangUp` (STT keeps talking for 5 s after the
  hang-up): `cancel`. `LosesFirstDelta` (LLM): `streaming`. `KeepAlive` (the LLM task lingers 3600 s
  after `Done`; the linger is longer than the suite, so tasks of earlier checks cannot exit inside
  the leak window and mask it): `no_task_leak`.
- `DeadCancel` (the cancel receiver is dropped, so the send fails loudly): exactly `cancel`.
  `IgnoresCancel` (the cancel is accepted into a channel nothing reads, so the barge-in is lost):
  exactly `cancel`. `DelayedCancel` (cancels reach the provider 1 s late): exactly `cancel`.
  `AckThenKeepsStreaming` (the ack is sent at once, the synthesizer keeps streaming the cancelled
  context): exactly `cancel`. `DiesAfterCancel` (the session closes with the ack, so the next
  context cannot be served): exactly `cancel`. `Stragglers(n)` (n audio events of the cancelled
  context arrive between the cancel and its ack): n = 2 passes, n = 3 fails exactly `cancel`.
  Mutation-checked: removing the follow-up context from the check makes the `DiesAfterCancel`
  test fail, and removing the quiet window and the follow-up together makes the
  `AckThenKeepsStreaming` test fail; the quiet window alone is covered by the follow-up, which
  also rejects stray events.
- `Truncated` (TTS sends only the first audio chunk): `audio_and_words`. `LateAudio` (the last audio
  of a context arrives after its `Flushed`): `audio_and_words` and `flush`.
- `NoIdleTimeout` (the deadline covers the first event only, a later stall is waited out for
  good): exactly `timeout`, for all three kinds. A mock whose own deadline is 500 ms or 1.5 s
  instead of the configured 2 s fails exactly `timeout` (too short: cuts off the slow case, or
  times out early).
- `FrozenCounter` (LLM harness whose `events_sent` never advances): exactly `cancel`.
- `LeadingSilence` (every word reported 80 ms late) is conformant and passes the whole TTS suite.
  A TTS mock that is not paced (a burst of audio queued in the channel when the first chunk is
  read, as a fast synthesizer produces) also passes the whole suite, `cancel` included.
- The mocks without `with_timeout` fail exactly `timeout`. With a connect latency above the
  deadline they report `Core(Timeout)` at the deadline
  (`mock_connect_latency_above_the_deadline_is_a_timeout`).

The TTS mock is paced in most contract tests (one 40 ms chunk per 40 ms), like a real
synthesizer. The suite does not depend on it: the cancel check excludes what is already buffered,
and `a_synthesizer_faster_than_real_time_passes_the_tts_suite` runs the unpaced mock.

Mock support added for the suite (all opt-in; existing behaviour and tests are unchanged):
`with_timeout(Duration)` gives a mock an adapter-side deadline (a slow connect, or an event owed
for longer than the deadline, becomes `Core(Timeout)`); `StreamFault::ConnectStall` and
`LlmFault::ConnectStall` make a connect slow; `clear_script()` resets queued state;
`StreamFault::ProtocolError` injects a protocol violation; `MockLlm::events_sent()` counts the
response events handed to streams. One behaviour change: when the caller drops an STT session's
input, the interims still owed are discarded (finals requested by a `Finalize` are still
delivered), so a hang-up does not deliver a stale interim
(`tests/mock_stt.rs`: `hanging_up_discards_the_interims_still_owed` and
`hanging_up_before_the_first_interim_yields_no_transcript_at_all`, with interim latency, so that
interims are owed at the hang-up).

**Plugging in an adapter (as done for Deepgram, OpenAI and Cartesia).** Each adapter's test
crate depends on `brisvox-sim` (dev-dependency) and implements the matching harness:
1. Start a local mock server that speaks the provider's wire protocol (WebSocket for Deepgram
   and Cartesia, HTTP/SSE for OpenAI). The adapter connects to it with a deadline of
   `CONTRACT_TIMEOUT`.
2. Implement `SttHarness`/`LlmHarness`/`TtsHarness` for a wrapper holding the adapter and the
   server handle: `provider()` returns the adapter (`Arc`), `clock()` the shared clock, `reset()`
   clears the server's script, `script()` makes the server send the given transcripts or deltas,
   and `inject(Fault)` makes the server refuse the next connect with a status, drop the connection
   after N events, send a protocol violation, delay event N, or delay the connect
   (`Fault::ConnectStall`). `LlmHarness::events_sent()` returns how many response events the
   server has sent so far (cumulative): the cancel check reads it to tell whether generation went
   on after a drop.
3. Write a test: `let report = stt_contract(&harness).await; assert!(report.passed(), "{report:#?}");`
   under real time (`SystemClock`) with the server's own clock, or under paused time when the
   server runs in-process on the same runtime.
4. Add the adapter's own negative tests the way `tests/contract.rs` does (a double that breaks
   one behaviour must fail exactly the check it targets) so the suite is known to bite.

First worked example: `crates/brisvox-providers/tests/openai/`. Its mock is a hand-written HTTP/1.1 chunked SSE server over a loopback `TcpListener` (`mock.rs`); `contract.rs` is the harness (real `SystemClock`, 400 ms deadline, `Connection: close` per response so the leak checks see a quiet runtime) plus a negative test (an adapter with 3x the deadline fails `timeout`); `protocol.rs` holds the wire-level tests; `live.rs` is the opt-in live test.

Limitation: the checks observe behaviour, not wire bytes. Wire-level facts (endpoints, message
shapes, auth headers) stay in each adapter's own tests and `docs/providers/`.

## Tool calling through the engine, scripted and live

`crates/brisvox-engine/tests/tool_scenarios/` runs one set of scenarios (parallel calls, a chain, a
booking that the model repeats, invalid arguments, a slow tool with its filler, a tool timeout, a
barge-in during the tools, and the speech-to-speech stage) against the whole engine path
(`LlmRunner` + `ToolEngine`) over five scripted providers (the simulator's `MockLlm` plain and with
interleaved argument fragments, and the real OpenAI-compatible, Anthropic and Gemini adapters over
loopback servers that stream each vendor's parallel-call format) and, with `BRISVOX_LIVE_TESTS=1`,
over the real providers with loose assertions (the loose assertions are also run against the
scripts, so they are known to hold before a key is involved). The pattern to copy for a new
live-capable behaviour: one scenario function over a `Provider`, an exact branch for the script
and a loose branch for a real model. Catalogue, results and the command for the live runs:
`docs/TOOLS.md`, "Verification: parallel and chained calls".

## Listen under load: talk to the bot while N other calls run

`scripts/listen_under_load.py` answers "how does MY call sound when the box is busy?". It starts,
confined to the same CPU cores (default 4; Linux `sched_setaffinity`, Windows `ProcessorAffinity`):

- `brisvox-server` with your agent file (real providers) and the WebRTC listener: your own call,
  in a browser at `http://localhost:8000` (media on this machine's LAN address: browsers ignore
  loopback ICE candidates);
- `brisvox-bench engine-server --models`: the production engine over mock providers with the real
  Silero VAD and Smart Turn models, so every background call costs what a real call costs
  on this machine (the providers' own work happens elsewhere anyway);

and holds N background calls per step with the capacity client pinned to other cores, printing
their p50/p95 voice-to-voice, answered turns and engine CPU while you talk. Enter moves to the
next step (`--auto` runs unattended). Keys come from the environment only.

```sh
cargo build --release -p brisvox-server --features provider-deepgram,provider-openai-compatible,provider-cartesia,webrtc,smart-turn-ort
cargo build --release -p brisvox-bench --features models
set -a; . ./.env; set +a
python scripts/listen_under_load.py --agent-file my-agent.toml --calls 25,100,200,300 --cores 4
```

Smoke run 2026-10-13 (Windows, `--auto`, no own call): 10 calls p95 836 ms, 40 calls p95 848 ms,
every turn answered, engine 0.12 / 0.34 cores.
