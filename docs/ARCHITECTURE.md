# Architecture

Status: design target. Update this file whenever code diverges (code wins, then fix the doc).

## Goals
1. Lower voice-to-voice latency than Pipecat with identical providers (see LATENCY.md).
2. Hundreds of concurrent sessions in one process, with per-session isolation.
3. Correctness by construction: epochs, exhaustive frame handling, deterministic simulation.

## Crates
```
crates/
  brisvox-core        Frame enum, Processor trait, Pipeline, Clock, epochs, TurnTimeline, errors
  brisvox-audio       codecs (μ-law/A-law/PCM16), resampling, buffers, output pacing
  brisvox-inference   dedicated CPU pool, ONNX Runtime models (Silero VAD, Smart Turn)
  brisvox-agent       turn-taking state machine, interruption policy, context, tool engine,
                      TTS segmenter, speculative generation, flows
  brisvox-providers   STT / LLM / TTS traits + adapters (cargo features per provider)
  brisvox-transport   transport trait + websocket, twilio, telnyx, webrtc, (sip)
  brisvox-engine      the production voice pipeline: stages (pump, STT, turn driver, LLM runner,
                      TTS, transport-out), EngineConfig, TaskGuard/GuardedSender/EventSlot/Offload,
                      PlayoutBook. Design: PIPELINE.md (Wake/EventSlot self-wake contract: sections
                      0-2).
  brisvox-server      multi-tenant session manager, admission, drain, config, HTTP API, binary
  brisvox-sim         mock providers, deterministic simulator, fixtures, leak checks
  brisvox-bench       benchmark harness binary
sdk/python, sdk/ts    out-of-process agent SDKs
bench/                pipecat baseline, results
```
Dependency direction: core ← audio ← inference ← agent ← providers/transport ← engine ← server.
`engine` sits above providers and transport and below `server`. `sim` depends only on core, agent
and providers (never on `brisvox-engine`, `transport` or `server`): the engine's own tests
dev-depend on `sim` (hybrid tests, `TaskCounter`) and run its suite through a `SessionFactory`, so
sim stays the untouched reference baseline and there is no cycle. `bench` may depend on anything.
Nothing depends on `sim` or `bench` except dev-deps.

## Time
All logic reads time through `brisvox_core::Clock` (`SharedClock = Arc<dyn Clock>`): `now()` (tokio
monotonic `Instant`), `wall()` (logs only), `sleep_until()`/`sleep()` (concrete `tokio::time::Sleep`,
no allocation). `SystemClock` in production; `SimClock` on a paused-time tokio runtime in
simulations (fixed wall origin 2026-01-01Z). Direct `Instant::now`/`SystemTime::now`/tokio sleeps
are `disallowed-methods` in `clippy.toml`.

## Errors and tracing
Per-crate `thiserror` enums wrapping `brisvox_core::Error`; spans `session{id}` ⊃ `turn{epoch}` ⊃
`stage{name}`; credentials only as `Secret<T>`. Details: `docs/TRACING.md`.

## Runtime model
- tokio multi-thread runtime: all I/O (sockets, provider streams, timers, pipeline routing).
- Inference pool: N dedicated OS threads (default = logical cores / 2, at least 1), bounded queue.
  Anything heavier than ~50 µs (VAD, Smart Turn, heavy DSP) goes here. Never on tokio workers.
  Two-lane queue: VAD windows (`Priority::High`, 60-100 µs, one per call every 32 ms)
  before everything else.
- Smart Turn executor: a second, separate `InferencePool` (`brisvox-turn-<i>` threads,
  default as many as the VAD pool, `BRISVOX_SMART_TURN_THREADS`, 0 = share the VAD pool) that
  runs only the Smart Turn analyses (20-25 ms each) and the model's warm-up. A 25 ms analysis on
  a VAD pool thread made the windows of other calls queue behind it (mean wait 5 ms, p95 V2V over
  the SLO at 250 calls with 2.3-3.2 of 4 cores busy); on threads of their own it cannot. Queue:
  32 jobs per thread, reject newest. A refused or late analysis is a missing verdict and the turn
  ends at its fallback deadline (the same degradation as a saturated pool before), the VAD is
  never involved. Server and benchmark build it with one function
  (`brisvox_engine::offload::start_smart_turn_executor`).
- One session = a supervised group of tasks sharing a `CancellationToken`.

## Frame flow (one session)
```
transport.in → decode/resample → VAD(pool) → turn SM ──► STT stream ─┐
                                   │                                 │ transcripts
                                   └──► Smart Turn(own executor) ──► end-of-turn decision
                                                                     │
                     context ◄── tool engine ◄── LLM stream ◄────────┘ (speculative start allowed)
                                                    │ tokens
                                         TTS segmenter → TTS stream → pacing → encode → transport.out
                                                                         │ word timestamps
                                                       played-text truncation on interrupt
```
System frames (interrupt, cancel, VAD events) travel on each processor's priority queue and
overtake data frames.

## Routing
Code: `brisvox_core::{queue, processor}`. Each processor is a task (`run_processor`) owning one
`Inbox` with four bounded lanes: {downstream, upstream} × {system, data}. Neighbours hold a
`FrameSender` bound to a direction; `ProcessorContext::push(frame, dir)` sends to the neighbour in
that direction (dropped at the pipeline edge).
- Receive order: any system frame first (downstream lane, then upstream), else data lanes,
  alternating directions when both have data. Within a lane: FIFO.
- Lane buffers are allocated once (`VecDeque::with_capacity`); send/recv never allocate. One
  `std::sync::Mutex` per inbox, never held across `.await`; waiting via `tokio::sync::Notify`.
- Capacities: `QueueConfig` (default 32 system / 32 data per direction; min 1). Keep data lanes
  small: queued audio is latency.
- Overflow (data lanes only; system lanes always backpressure): `OverflowPolicies` per inbox
  configures audio and metrics; everything else is always backpressure.
  `DEFAULT` = audio backpressure, metrics drop-newest. `REALTIME_INPUT` (stage after
  transport.in) = audio drop-oldest (evicts the oldest queued audio frame, never reorders),
  metrics drop-newest. Drops are counted in `QueueStats`.
- Closing: `recv` returns `None` once all senders are gone and the inbox is drained; sending to a
  dropped inbox fails with `ChannelClosed` (also releasing blocked senders). In a bidirectional
  chain neighbours hold senders into each other's inboxes, so drop-based close never fires:
  pipelines stop via `Stop`/`Cancel` system frames or the `CancellationToken`.
- Cancellation: `run_processor` holds one `cancelled()` waiter for the task's life plus an atomic
  check per frame; it interrupts `process` wherever it is parked. `Processor::process` must
  therefore be cancel-safe.
- Upstream rule: upstream traffic must be rare or lossy (metrics are drop-newest). Two neighbours
  each blocked pushing backpressure frames into the other's full data lane would deadlock.
- Priority detail: downstream system frames are taken before upstream ones; blocked senders on
  one inbox are woken together with no FIFO order between them (one sender per direction is the
  normal wiring).
- Known limitation: while `process` is blocked pushing into a full neighbour, newly
  arrived system frames wait until that push completes. Interrupt handling must interrupt the
  blocked push and purge stale frames (an `Inbox::retain`-style API is needed).
- Cost (release, Windows, 640-byte frames, measured by perf audit): ~100 ns per hop SPSC;
  ~0.4–0.6 µs per frame through a 5-processor chain on 1–4 worker threads. Zero allocations per
  frame.

## Epochs
Every turn has an epoch number. An interrupt bumps the epoch; all processors drop frames whose
epoch is older. This makes "stale audio after barge-in" structurally impossible.

Epochs are turn numbers: the turn state machine starts a new epoch at every user turn start and
sends `Interrupt` only for a barge-in (bot speaking/generating).

Mechanism (code: `epoch::EpochGate`, `run_processor`, `ProcessorContext::push`,
`Inbox::{purge_stale, epoch_watch}`), applied by every task loop (processors, fan-out, merge):
1. Gate on dequeue: an `Interrupt { new_epoch }` newer than the task's epoch advances it; an
   interrupt that is not newer is a duplicate and dropped (no echo when it arrives from both
   sides); any other frame with `Frame::is_stale` is dropped before `process`.
2. Purge on advance: the task's inbox drops queued stale data-lane frames.
3. Outgoing: `push` drops frames older than the task's epoch.
4. Blocked push: when `push` must wait for a full neighbour, it also watches the task's own
   inbox; the sender side records the newest enqueued interrupt epoch (atomic `fetch_max`), and
   a push whose frame becomes stale is abandoned at once, so an interrupt is never delayed
   behind backpressure. Fast path (space available) registers nothing.
5. Originating: a processor that pushes a newer `Interrupt` advances its own context epoch; the
   task loop then adopts it and purges its inbox. `push` also drops frames older than the newest
   interrupt already waiting in the task's own inbox (fast path: one atomic load).
`ToolResult` frames are exempt everywhere (SideEffect safety). `Processor::epoch_policy()`
defaults to `DropStale`; `KeepAll` opts out (recorders). The layers are redundant on purpose:
tests were mutation-checked to fail only when every layer covering a path is removed (e.g. the
merge path needs merge gate + push watch + push-side advance all removed).

## Providers
Traits in `brisvox-providers` (details in `docs/PROVIDERS.md`): channel-based sessions
(bounded, backpressure; dropping a handle cancels), `warm()` default methods.
- `SttProvider::connect -> SttSession { input: Audio | Finalize, events: Transcript { is_final,
  speech_final, from_finalize } | Error }`.
- `LlmProvider::stream(LlmRequest) -> LlmStream` of `TextDelta | ToolCallDelta | Done { finish,
  usage } | Error`.
- `TtsProvider::connect -> TtsSession { input: Text | Flush per ContextId, cancel: priority
  channel, events: Audio | Word | Flushed | Cancelled per ContextId | Error }`.
Adapters are cargo features (`provider-deepgram`, ...) to keep builds small. Mocks with latency
profiles and fault injection live in `brisvox-sim`.

## Server
Session manager (spawn/supervise), admission control (CPU/pool pressure + per-provider
semaphores), graceful drain, health endpoints, metrics, config loading.

Session manager (code: `brisvox_server::session`): bounded admission
(`try_admit` → `SessionSlot`, typed `AdmissionError::{AtCapacity, ShuttingDown}`, nothing
queues; a slot reserved before drain is honoured, `SessionSlot::spawn` cannot fail, and the
shutdown waits for outstanding reservations); one tokio task per session with the future and
the classification of its result (error `Display`, output/error drop) under `catch_unwind`, so a
panic ends only that session (`SessionEnded::Panicked`, counted); a drop guard owned by the task removes the
registry entry on every exit path (ok, error, panic, cancel, abort, runtime drop). Each session
gets a child of the manager-wide `CancellationToken` (cancelled only by `shutdown`;
`cancel_all` cancels the sessions registered at that moment, not later ones). `shutdown(ShutdownPolicy)` runs
drain → cancel → abort, each stage timed on the injected `Clock`, and reports leftovers. The
manager is generic over the session future; the engine plugs in through `SessionRunner`
(adapter for `run_call`). The server binary uses mimalloc as global allocator
(cross-thread `Arc<str>` frees ~2x faster, `examples/alloc_bench.rs`).

Process lifecycle (code: `brisvox_server::{settings, health, lifecycle}`,
`docs/OPERATIONS.md`): settings from `BRISVOX_*` environment variables; a minimal HTTP/1.1
listener (no web framework) serves `/healthz` (always 200 while alive) and
`/readyz` (200 ready, 503 `full` or `draining`, JSON with active/reserved/max). `SIGTERM`/`SIGINT`
(Ctrl-C/Ctrl-Break on Windows) starts `lifecycle::drain`: `begin_drain` at once (readiness
flips, `try_admit` returns `ShuttingDown`), wait up to the drain timeout on the injected
clock (a second signal skips the rest), then `SessionManager::shutdown` cancels and aborts what
is left. The exit status says how it ended (0 drained, 3 cancelled, 4 aborted, 5 leaked, 1
startup failure); the runtime is then shut down with a 1 s bound (`RUNTIME_SHUTDOWN_TIMEOUT`) so
a stuck `spawn_blocking` closure cannot hold the exit.

Agent definitions (code: `brisvox_server::config`, `docs/CONFIG.md`): one TOML file per
agent (prompt, language, tools by name, turn/interruption/speculation settings, one provider per
stage with its options table). Unknown keys are rejected; every error names the file, the key
path and the line/column; cross-field checks (provider feature compiled in, provider language
and sample-rate support, policy-specific keys) report together; secrets come only from the
environment variables the file names (`api_key_env`) into `Secret<String>`. Pure functions map
the result onto `EngineConfig` and the adapters' `XxxConfig` structs.

## SDK boundary
External agent logic connects over a versioned protocol (`docs/AGENT_PROTOCOL.md`,
crate `brisvox-agent-link`). The audio path never awaits SDK code; remote tools are just
tools with timeouts.

- **One link per call**, opened by the server (`attach`, from the runner: `EngineRunner::
  with_agent_link`, `BRISVOX_AGENT_URL`; the agent process can be picked per call by a route
  named in the start message) before the pipeline starts. Three tasks per link: a
  connection task (owns the socket and the queues; runs a writer, control messages before
  events with every write bounded, and a reader that applies commands without ever waiting on
  the call; reopens a lost connection of an agent that declared `resume` with bounded backoff
  and a `session.start` carrying `resume`), an event pump (the call's `EngineEvents` ring,
  shared with the RTVI bridge) and an operations task (`end`, `transfer`; `transfer`
  goes through the call's own carrier binding, so the runner gives a call the telephony state
  even when its definition names no telephony tool).
- **Commands use the engine's own inputs** (`brisvox_engine::agent_input`):
  - *Bot lines* (`say`) go to the turn driver, the epoch authority, as a pure machine input
    `TurnInput::BotTurn { interrupt }`. From `Idle` (no candidate) the machine starts a bot turn
    in a new epoch (`TurnAction::StartBotTurn`, state `AwaitingResponse`); with `interrupt`, from
    `AwaitingResponse`/`BotSpeaking` (no candidate), it cuts the response first (`Interrupt`,
    then `StartBotTurn`). Otherwise the driver holds the line (at most 8, in order) and offers it
    again after every frame it handles. The driver sends the line downstream as an `LlmText` of
    the new epoch; the LLM runner (nothing upstream of it produces LLM text) settles the context
    as at a turn end and speaks it through the round task's canned-line path (`Turn::say`, the
    same path as the fallback line): in the context at once, registered in the `PlayoutBook`,
    cut to what was heard on a barge-in. Its audio drives the machine like any response
    (`BotAudioStarted`/`Ended`), so the caller can barge in on it.
  - *Conversation edits* (`context.update`, `tools.set`) are a bounded channel the runner drains
    at its safe points only (a user turn's end, a bot line, a speculative start), after the late
    tool notes: never while a round task can touch the context. `SetTools` narrows the offered
    tools (`Shared::offered`); a call to a tool not offered is answered `unknown_tool` without
    running.
  - `end` resolves `AgentLink::ended` once the farewell line was played and the bot is silent;
    the runner then stops the call gracefully. `transfer` goes through the call's
    `TelephonyOps`.
- **Remote tools** are registered next to the builder's own tools (the registry is copied with
  `ToolRegistry::filtered`, the tool engine rebuilt with the same `EngineConfig`); their
  `Tool::call` is a `tool.call`/`tool.result` RPC with the engine's ReadOnly/SideEffect rules.
- **Failure never fails the call** (fallback: the agent definition). See the protocol document.

## Pipeline
Code: `brisvox_core::pipeline`. `Pipeline::new(name).then(p).then_pipeline(inner)…`; processors are
type-erased once when added (boxed spawn closure), never per frame. Nested pipelines are
flattened at `start`, so nesting adds no hop and no task. `start(links, &parent_token)` creates
one inbox + task per processor, links neighbours both ways, connects edges to `Links`, then sends
`Start` downstream.
- Graceful stop: `RunningPipeline::stop(timeout)` sends `Stop`. `run_processor` treats a
  downstream `Stop` specially: it processes everything already queued (in receive order), then
  `Stop` (which the processor forwards), then exits. Upstream neighbours therefore finish first,
  and frames pushed upstream into a stopped neighbour are discarded (debug-logged, not an
  error). The whole stop (including sending `Stop` into a full system lane) is bounded by
  `timeout`; on expiry the pipeline is cancelled, tasks aborted, and `Error::Timeout` (or the
  root-cause failure, if any) returned. A Stop drain stops early on cancellation or a queued
  `Cancel` frame.
- `Cancel` frame: processed (forwarded) immediately, then the task exits; queued frames dropped.
  `RunningPipeline::cancel()` / the parent token: every task exits at once.
- Supervision: `wait()` returns the root-cause processor error or
  `Error::ProcessorPanicked(stage)`; the first failure cancels the rest. `ChannelClosed` from a
  neighbour of the failed processor is secondary and replaced by the real cause. Cancel-safe
  (failure kept on the handle). A panic stays inside its task (session isolation).
  Dropping a `RunningPipeline` cancels and aborts its tasks.
- Leak checks: `brisvox_sim::leak::TaskCounter` (tokio `num_alive_tasks`) waits on the `Clock`
  until the runtime is back to its baseline task count.

## Parallel branches
`Pipeline::then_parallel([branch pipelines])` wraps the branches in a fan-out unit and a merge
unit (code: `pipeline/parallel.rs`); `start` wires the whole graph (units built first, then
linked), nested pipelines and nested parallel blocks included.
- Every frame entering the block goes to every branch (data clones share buffers). At most 64
  branches; an empty branch or block is an error, an empty nested pipeline is skipped.
- Each branch reaches the merge (and, upstream, the fan-out) through a sender tagged with its
  index (`Inbox::sender_from`/`recv_from`). Copies of broadcast System/Control frames collapse
  via a shared bounded ledger (256 entries/direction) that records, per broadcast, which branches
  have passed it (u64 mask): a branch's copy matches the oldest equal broadcast *it* has not
  passed yet, so a branch that swallows frames cannot make later broadcasts disappear.
- `Stop` is forwarded once every branch has passed it on (then the merge drains and ends);
  `Cancel` on the first copy, after which the merge forwards nothing more (the next stage is
  shutting down) and ends once every branch has cancelled; every other broadcast frame on the
  first copy, so an interrupt is never held back by a slow branch.
- Frames originating inside a branch, and all data frames, pass through: data broadcast to N
  pass-through branches arrives N times, so branches that should not emit data must consume it.
- Upstream is symmetric (merge broadcasts to branch tails, fan-out collapses copies).
- Branches must forward the System/Control frames they receive (the general processor
  contract). A branch that swallows them leaves incomplete ledger entries (evicted oldest-first,
  warned once); other branches' copies are still forwarded exactly once.
- Head-of-line: broadcasts go to branches one after another, so a full branch inbox stalls the
  fan-out for every branch; the merge's upstream broadcast multiplies upstream load by N. The
  pairwise upstream/downstream backpressure caveat (see Routing) applies at fan-out↔head and
  tail↔merge.

## Frame table
Code: `brisvox_core::frame`. `Frame` ≤ 64 bytes (compile-time assert; exactly 64 on x86-64 Windows,
measured; Linux/macOS checked by CI once CI covers them).
Direction: ↓ downstream (transport.in → transport.out), ↑ upstream (back towards transport.in).
Overflow policies are the *intended* ones; the processor core implements and tests them.

| Variant | Level | Dir | Producer | Consumers | Overflow policy |
|---|---|---|---|---|---|
| `System::Start` | System | ↓ | pipeline/session | all processors | never dropped (system queue) |
| `System::Stop` | System | ↓ | session | all processors | never dropped; drain data first |
| `System::Cancel` | System | ↓↑ | session, fatal error | all processors | never dropped; drop queued data |
| `System::Interrupt{new_epoch}` | System | ↓↑ | turn state machine | all processors (purge older epochs) | never dropped |
| `System::UserStartedSpeaking{at}` | System | ↓↑ | VAD | turn SM, interruption policy, timeline | never dropped |
| `System::UserStoppedSpeaking{at}` | System | ↓↑ | VAD | turn SM, Smart Turn, timeline | never dropped |
| `System::UserSpeechFalling{at}` | System | ↓ | VAD (opt-in: `VadSetup::with_early_analysis`) | `TurnDriver` (consumes it, never forwards; starts an early Smart Turn analysis); other stages forward it unchanged | never dropped; sent once per fall, always BEFORE the stop of the same fall, `at` = exactly that stop's time (needs `speech_pad <= min_silence`, else none is sent); a fall cancelled by speech sends no stop |
| `System::Error(Box)` | System | ↑ | any processor | session supervisor | never dropped |
| `System::Wake{token}` | System | self | a helper task of the stage, via `ProcessorContext::self_sender` | the same stage only | never dropped or purged; never forwarded (`push` discards it); payload travels in the stage's `EventSlot` |
| `Control{UserTurnStarted}` | Control | ↓ | turn SM | STT, context, timeline | backpressure; never dropped by overflow, subject to epoch purge |
| `Control{UserTurnEnded}` | Control | ↓ | turn SM | LLM runner, context, timeline | backpressure; epoch purge |
| `Control{LlmResponseStarted/Ended}` | Control | ↓ | LLM stage | TTS segmenter, context | backpressure; epoch purge |
| `Control{TtsStarted/Ended}` | Control | ↓ | TTS stage | pacing, context (played text) | backpressure; epoch purge |
| `Control{Flush}` | Control | ↓ | turn SM, LLM stage | segmenter, TTS, pacing | backpressure; epoch purge |
| `Control{UtteranceFinal}` | Control | ↓ | STT stage | turn SM | backpressure (user side: KeepAll) |
| `Control{UtteranceStarted{utterance}}` | Control | ↓ | TTS stage, right before the first audio of each utterance | transport-out stage | backpressure; epoch purge; same data lane as the audio, so it stays in order with it |
| `Data::Audio` (input) | Data | ↓ | transport.in | decode/resample, VAD, STT | drop oldest (stale input is useless) + metric |
| `Data::Audio` (output) | Data | ↓ | TTS | pacing, transport.out | backpressure (paced at real time) |
| `Data::Transcript` | Data | ↓ | STT | turn SM, context | backpressure |
| `Data::LlmText` | Data | ↓ | LLM stage | segmenter, context | backpressure |
| `Data::ToolCallDelta(Box)` | Data | ↓ | LLM stage | tool engine | backpressure |
| `Data::ToolResult(Box)` | Data | ↓ | tool engine | context, LLM runner | backpressure; **exempt from epoch purge** |
| `Data::Dtmf` | Data | ↓ | transport.in | turn SM / flows | backpressure |
| `Data::Word(Box)` | Data | ↓ | TTS stage | played-text tracker (`brisvox_agent::played`) | backpressure; **exempt from epoch purge** |
| `Data::Metric` | Data | ↑ | any stage | timeline / metrics sink | drop newest + counter (observability must not stall audio) |

Epochs: System frames carry none (never stale). Every Control and Data frame carries the
`TurnEpoch` (u32) it was produced in. `Frame::is_stale(current)` is the purge predicate for interruptions:
older epoch → drop, except `ToolResult` (a SideEffect call may already have run; its result must
reach the context, TOOLS.md item 8). The turn state machine owns the current epoch; `Interrupt`
carries the new one and every later frame (starting with `UserTurnStarted`) is tagged with it.
`AudioFrame` fields are private: the buffer is always whole PCM16 samples; `sample_count()` and
`duration()` are derived, never stored; `seq` is u64. `Debug` of frames prints sizes only, never
samples or user/LLM text.

## Production pipeline (`brisvox-engine`)
One call is `brisvox_engine::run_call(session, builder, cancel)` over a `TransportSession`: it
builds the stages (`build_session_pipeline`), starts them, and runs the transport pump. The design
and the as-built notes of every stage are in `docs/PIPELINE.md`; this is the overview. The
reference pipeline of `brisvox-sim` stays untouched as the golden baseline the engine is tested
against (`docs/TESTING.md`, "How the production pipeline is proven").

### Stages
| # | Stage | Epochs | Does | Heavy work |
|---|---|---|---|---|
| - | pump (task, not a stage) | retags input to epoch 0 | drains `TransportSession::recv`: audio into stage 1 (drop-oldest, counted), marks, positions and leads into the `PlayoutBook`, end marks as upstream `TtsEnded`, hang-up as `Stop` | none |
| 1 | `AudioIn` + VAD worker | KeepAll, epoch 0 | resamples to 16 kHz and forwards at once; a recycled copy goes to the VAD worker, which batches windows into one job on the pool and sends backdated `UserStartedSpeaking`/`UserStoppedSpeaking` (opt-in `UserSpeechFalling`) | VAD on the inference pool |
| 2 | `SttStage` | KeepAll, epoch 0 | connects at start; audio to the provider (`try_send`, drops counted); at a VAD stop forwards the stop, then sends `Finalize`; exactly one `UtteranceFinal` per `Finalize` (real, or synthetic on timeout or a dead session) | none |
| 3 | `TurnDriver` | KeepAll | the **only epoch authority**: owns the `TurnMachine`, the Smart Turn ring, the finals gate, the response backstop and the barge-in guards; emits `Interrupt`, `UserTurnStarted`, the final `Transcript`, `UserTurnEnded` | Smart Turn on the pool |
| 4 | `LlmRunner` | DropStale | one round task per epoch: request (`build_window`), streamed text, tool loop, fillers, played-text truncation at the next turn (`docs/TOOLS.md`, "Tools in the production pipeline") | a large assembler finish or request build on the pool |
| 5 | `SegmenterStage` | DropStale | speakable chunks as raw slices of the response (the first after one word) | none |
| 6 | `TtsStage` | DropStale | one provider context per round (utterance), audio cut into 20 ms frames at the call's output rate, word timings, `TtsEnded` | none (per-chunk resampling, measured < 50 us) |
| 7 | `TransportOut` | KeepAll, filters itself | frames to `TransportCommand`s: audio, marks, `Clear`; `FirstAudioOut`, upstream `TtsStarted` | none |

Every queue is bounded with a stated overflow policy (`docs/PIPELINE.md` section 1); every helper
task is a `TaskGuard` cancelled with its stage; every network wait has a timeout on the injected
`Clock` (connect 5 s, LLM first token 8 s, idle 15 s, `Finalize` answer, clear send).

### Epoch ownership
- The `TurnDriver` alone advances epochs: a new epoch at every user turn, `Interrupt{new}` only for
  a barge-in, always before `UserTurnStarted{new}`. Stages 1 and 2 (the user side) stay on epoch 0
  and keep everything; stages 4 to 6 (the bot side) drop stale frames; `TransportOut` drops and
  counts them itself.
- Helper tasks (LLM rounds, TTS readers, timers) send through a `GuardedSender` that checks the
  stage's epoch right before each send, so a cancelled round cannot leak a late frame.
- `ToolResult` frames are never purged (SideEffect safety, `docs/TOOLS.md` item 8).

### Clear on epoch advance
The transport's own epoch ledger advances only through `Clear{epoch}`, but the turn epoch advances
every turn. `TransportOut` therefore sends `Clear{epoch}` before the first audio of any epoch newer
than the last one it cleared to (on an idle transport it cuts nothing; `Clear` is idempotent for
an epoch). Without it the second turn's audio would be dropped as stale by the transport. On a
barge-in the driver clears first, on the fast path (below), and `TransportOut`'s own `Clear` is a
no-op.

### No second pacer
`TransportSession::open` already contains the `OutputPacer` that releases one 20 ms frame per
tick. The pipeline's tail is `TransportOut`, which only translates frames: there is no pacing
stage in the engine (`docs/PIPELINE.md` D8; a test shows the session's pacer alone spaces the
output).

### PlayoutBook
`PlayoutBook` (engine, `playout.rs`, std `Mutex` never held across an await, a `watch` for settle
notification, bounded) is the record of what the caller heard, keyed by `(epoch, round)`: the
runner writes each round's context message index, the TTS stage its utterance id and the exact
text it sent, `TransportOut` and the pump the word timings, released positions, echoed marks,
clears, losses and client leads. At the next turn (and at stop) the runner waits, bounded, for the
cut utterances to settle and cuts the context to what was heard (`PlayedText::heard`,
`apply_truncation`): the context holds what was played, and only that.

### Side channels
Frames are the only data path, except for these shared objects (the first three are listed in the engine design notes):
1. `PlayoutBook` (above).
2. The `CommandSender` the driver uses for the **fast-path `Clear`** at a barge-in: applied inline
   in the transport, bypassing the command queue, before the `Interrupt` travels downstream
   (`docs/PIPELINE.md` section 3; the book learns of the clear before the command).
3. `Arc<Mutex<Context>>`, the conversation, written by the runner and read by the sim probes.
4. Observation only: the `TimelineRecorder` (every stage marks its turn events; the VAD worker and
   the STT stage mark the open user turn) and the RTVI event observer (`Observed<P>` on
   five stage inputs, a drop-oldest broadcast ring). Neither can block or reorder a frame.

### Multilingual calls

An agent with `[languages]` (`docs/CONFIG.md`) follows the caller from one language to another
inside one call. The state is a `LanguageTracker` (`brisvox_engine::language`), one per call,
shared by four stages:

- `SttStage` collects the final segments of each utterance with the language the provider
  reported (`SttEvent::Transcript::language`) and hands the utterance to the tracker right before
  it emits the `UtteranceFinal`, so the decision precedes the turn. No language reported: the
  tracker detects it from the text (`whatlang`, restricted to the listed languages).
- The tracker switches after N consecutive utterances in another listed language, or at once on a
  confident provider report. Short utterances are no evidence.
- The language of a response is read **once, when its turn starts** (`LanguageTracker::language_for(epoch)`
  locks it for the epoch). `LlmRunner` pins `Reply in <language>.` into the provider request,
  `SegmenterStage` cuts with that language's abbreviations, and `TtsStage` normalizes in that
  language and, if its provider session speaks another voice or model, replaces the session at
  the response's `LlmResponseStarted` (a TTS session's voice is fixed when it opens for Aura and
  ElevenLabs; the replacement reuses the ordinary connect path with its timeout and
  cancellation). A switch decided while a response plays shows in the next one; a speculative
  LLM response started before the switch is discarded.
- The playout book and the played-text truncation do not know about languages: what was heard is
  what the context keeps, in whatever language it was spoken.

### Latency budget (measured)
Engine overhead at p95, from the engine's own timeline in the real-time bench: gated turn end to
LLM request <= 3 ms, first TTS audio to transport command <= 2 ms, barge-in decision to applied
`Clear` <= 1 ms. All three are met by orders of magnitude (`docs/LATENCY.md`, "Engine stage times
and memory"); prewarming keeps every connect and warm-up off the first turn
(`tests/prewarm_first_turn.rs`).

### Known limits
- Measured on a shared Windows machine; the Linux numbers for the comparisons with Pipecat and LiveKit and the
  strict 2 ms scheduler-gap bound on a quiet machine are still to be recorded.
- Speculative LLM start is opt-in and off by default; the default-on variant is a
  follow-up.
- Pre-rendered filler clips play through the TTS stage's clip path, but only
  inside a live provider session; text TTS is the fallback.
- A mid-utterance barge-in has no end mark yet: its heard position is the first position report
  after the clear (`playout` module docs).
- Live providers are tested against recorded protocols and mocks; the live variants need keys. The bench's VAD is the energy detector and its Smart Turn a fixed model.
- The server binary hosts the transports, RTVI included, but has no tool registry
  or Smart Turn model yet. Every call's providers run behind the resilience layer
  (`brisvox_server::resilient`).
- RTVI `send-text` with `audio_response=false` and `llm-function-call` events are not supported
.

## Long-call context

`brisvox_agent::window` bounds the input that a *caller builds* for an LLM request, over a call of
any length. The full `Context` is never modified: each request gets a new, smaller `Context` built
by `build_window(ctx, &WindowPolicy, &dyn TokenEstimator, summary) -> Window`.

**Not yet wired into the running session.** Nothing in the live pipeline calls `build_window`:
the reference `respond()` in `brisvox-sim` (`crates/brisvox-sim/src/reference.rs`) still sends
the full history. Until that call site exists, the LLM input of a live call is not bounded, and
the claim above holds for the library only. Wiring it in is a follow-up; the window tests below
measure the `Window` value, not an `LlmRequest`.

- **Turn**: a user message plus everything after it up to the next user message (assistant text,
  tool calls, their results, late notes). Messages before the first user message form one turn.
  Leading system messages are the header, always kept first and never clipped.
- **Cut**: the window keeps the most recent turns, oldest dropped first. It keeps up to
  `WindowPolicy::keep_recent_turns` (minimum 1) turns and, within `max_tokens` (estimated tokens,
  header and summary included), shrinks that count as far as needed, never below the latest turn.
  A tool call and its results are always in the same turn, so a cut never splits them.
- **Clipping**: a message whose estimate exceeds `WindowPolicy::max_message_tokens` (default: a
  quarter of `max_tokens`) is sent clipped: its text is cut at a character boundary, the longest
  prefix that fits, and `CLIP_MARKER` (` [clipped]`) is appended. The transcript keeps the full
  text. Tool call arguments are not clipped, so a message whose arguments alone exceed the limit
  still does. The summarizer receives the same clipped messages.
- **Summary**: `Summary { text, covers_until }` covers the messages from the end of the header up
  to `covers_until` (an index in the full context that must be a turn start). The window inserts
  it as a system message (`SUMMARY_PREFIX`) right after the header, counts it in the budget, and
  clips it to `WindowPolicy::max_summary_tokens` (default: a quarter of `max_tokens`) if a caller
  passes a longer one.
- **Summarizing**: `needs_summary(ctx, policy, est, summary)` returns the message range that the
  window drops without a summary yet (or `None`). The range is at most one round long: whole turns
  whose sent size fits `WindowPolicy::max_summary_input_tokens` (default: `max_tokens`), always at
  least one turn. `refresh_summary(...)` loops over the rounds, calling a `Summarizer` (a trait
  returning a `BoxFuture`; no LLM implementation lives here) until the summary covers exactly the
  dropped turns. Each call gets the current summary, the clipped messages and the size limit
  `max_summary_tokens`, which it should compress to. Summaries are produced asynchronously by the
  caller; while a summary lags, `Window::uncovered` names the dropped messages it does not cover.
- **Summary limit**: `refresh_summary` never stores a summary whose message estimates to more than
  `max_summary_tokens`. It returns `WindowError::SummaryTooLarge` and keeps the previous summary.
  The window then stays within budget but leaves the newer dropped turns uncovered, and every later
  call repeats the same round, so the caller decides how often to retry. This is a deliberate
  choice over silent truncation, which would drop the newest facts of a summary.
- **Over budget**: the window only exceeds `max_tokens` when a piece that cannot be dropped is
  larger than the budget alone: the header (the prompt), or the latest turn after clipping. That
  piece is still sent, and `Window::over_budget` reports it. Callers should log it and shorten the
  prompt. The number of messages in one turn is not limited here.
- **Errors**: an invalid full context (`Context::validate`) or a summary that does not end at a
  turn start is rejected (`WindowError`); the transcript is left untouched.

Acceptance (`crates/brisvox-agent/tests/long_call.rs`): a 60-minute call of 360 turns (about 15
words from the caller, about 40 from the agent, a tool call with its result every tenth turn) is
fed turn by turn. After every turn the test refreshes the summary, builds the window, and asserts
that the estimated tokens stay within budget, `validate()` passes, the prompt comes first, the
latest caller message is present, tool calls are whole, the summary message in the window is
exactly `SUMMARY_PREFIX` plus the stored text, and the summarized user turns plus the window's
user turns equal all user turns in order. Each summarizer call of a turn must receive the summary
the previous call returned (the first one, the summary from before the turn), with `covers_until`
equal to the start of its range, and the limit `max_summary_tokens`. Three policies run: 4000
tokens with 8 recent turns (the summary covers 352 turns at the end), 4000 tokens filled by the
budget alone (at least 3500 tokens used, never more than 4000), and a tighter 1200-token window.

Further tests in the same file and in `window.rs`:

- A summarizer that extends its summary every call (`a_summarizer_that_keeps_growing...`): the
  summary is rejected once it outgrows its limit, the window stays within budget for all 360 turns,
  and the rejected summary replaces nothing.
- A summarizer that rejects inputs above 200 estimated tokens, after a full outage: the backlog
  is caught up in several rounds, with no single input above the limit.
- A 40 KB tool result in the latest turn: the window is clipped to within budget, not over it, and
  the clipped text stays on character boundaries.
- A caller-supplied 40 KB summary is clipped in the window, and the caller's summary is untouched.

Each of the new behaviours was checked by reverting its fix once: disabling clipping, the summary
limit, or the round limit, dropping the previous summary from the summarizer call, and leaving the
summary text out of the window message each made the matching tests fail.

## Conversation flows

`brisvox_agent::flows` (`flows.rs`, `flows/def.rs`, `flows/manager.rs`, `flows/render.rs`,
`flows/example.rs`) is the equivalent of the basics of Pipecat Flows. A conversation is a graph of
named nodes; the tool results of the LLM move it from node to node. It is data plus a state machine:
no IO, no tasks, and no tool execution of its own. Tools run in the existing `ToolEngine`, history
lives in the existing `Context`, summaries come from the existing `window::Summarizer`.

**Not yet wired into the running session.** The LLM runner is the caller; until it exists
the module is exercised by `crates/brisvox-agent/tests/flows.rs` against the real `ToolEngine`.

### Concepts

| Item | What it is |
|---|---|
| `Flow` | Immutable, validated, shareable (`Arc<Flow>`): nodes, global tools, a role shared by the nodes, the `ToolRegistry` it was validated against (`Flow::registry()`, the registry the engine must run). Built with `FlowBuilder::build(registry)`. |
| `Node` | A step: role messages (default: the flow's) and task messages (the prompt), the tools it offers (`ToolBinding`s), a `ContextStrategy`, `pre_actions`, `post_actions`, `respond_immediately`, `keep_last_user`. |
| `ToolBinding` | A registry tool as one node offers it, plus a `Transition` and what to capture. A flow-level `global_tool` is offered in every node; a node's own binding of the same name wins. |
| `Transition` | `Goto(node)` (fires on success), `Rules(Vec<Rule>)` (first `Condition` that holds: `Always`, `Success`, `Failure`, `ResultEquals`, `ResultPresent`, `ArgEquals`, JSON pointers), `Custom(fn(&ToolOutcome) -> Option<NodeId>)`. |
| `ContextStrategy` | `Append`, `Reset`, `ResetWithSummary`: what happens to the history when a transition enters the node. |
| `Action` | `Say(text)`, `EndCall`, `Custom{name, payload}`. Named by the flow, performed by the runner. |
| `FlowManager` | The per-call state: current node, generation, state map. One per call, driven by `&mut self` from the runner's round task. |

`FlowBuilder::build` rejects (`FlowDefError`): no nodes, duplicate nodes, unknown initial node, a node
without prompt text, a duplicate or unregistered tool in a scope, and a static transition target that
is not a node. `Custom` targets cannot be checked statically and are checked at run time.

### The runner protocol (per round)

```text
flow.start(&mut ctx)                     // once; installs the initial prompt; returns the pre actions
loop per LLM round:
    request  = (ctx, flow.tool_specs())               // the CURRENT node's tools, name order
    ids      = flow.next_tool_call_ids(&ctx, epoch, n) // NOT ctx.next_tool_call_ids (see below)
    push the assistant message of the response
    screened = flow.screen(calls)                       // tools outside the node are refused here
    exec     = engine.execute(screened.take_admitted(), epoch, &cancel).await
    applied  = flow.apply(&mut ctx, screened, exec, Some(&summarizer)).await?
    // results appended; applied.transition = Some(Entered{pre/post actions, respond_immediately})
    // when a transition ran; applied.respond says whether to run another round
```

`FlowManager::transition_to(ctx, node, summarizer)` is the explicit transition for application code
(a DTMF key, a timer). It needs every tool call answered.

`apply` is two steps in one call, and the runner can take them apart:

```text
applied = flow.apply_results(&mut ctx, screened, exec)?   // sync: append, capture, DECIDE
// applied.pending = Some(node): play flow.pending_pre_actions() (the node's Say) now ...
entered = flow.resume_pending(&mut ctx, Some(&summarizer)).await?   // ... while this summarizes
// if ctx.context_replaced: drop the window Summary, the round->msg_index map, play-out indices
```

After the decision `flow.pending_transition()` is set until the node is entered. While it is set
the flow offers no tool (`tool_specs()` is empty and `screen` refuses every call), so a dropped
`apply` (barge-in, session cancel) cannot leave the node being left offering a SideEffect tool
again. The runner finishes with `resume_pending` (or the next `apply` does it: the round it
screens is refused whole, then the pending node is entered), or drops the decision with
`abandon_pending`.

### Rules and decisions

- **Message 0 is the node prompt.** `start` installs it, and every transition replaces it
  (`Append`) or rebuilds the context around it (`Reset*`). Role and task messages of a node are
  rendered into one system message joined by a blank line (`FlowManager::system_prompt()` is what
  message 0 holds). It is a leading system message, so `window::build_window` always keeps it. This
  is a deliberate difference from Pipecat, whose `APPEND` adds the new task messages at the end of
  the history: an appended system message is dropped by the window after enough turns and the model
  loses its instructions (test `the_node_prompt_survives_the_long_call_window_...`). The manager owns
  message 0; put global instructions in the flow role.
- **Strategies.** `Append` keeps the history and swaps message 0. `Reset` leaves the prompt alone
  (the LLM then sees no user message, so such a node should respond immediately and carry what it
  needs in its prompt through state placeholders). `ResetWithSummary` asks the summarizer for the
  newest history (whole turns from a user message, at most `summary_input_tokens`, the latest turn
  always; message 0 is not included) with `summary_max_tokens` as the size, and the context becomes
  the prompt plus a system message `FLOW_SUMMARY_PREFIX` + summary (clipped to the size). No
  history: plain `Reset`. No summarizer, an error, an empty reply or no answer within
  `summary_timeout` (default 8 s, on the injected clock): the transition still happens and falls back
  to `Append`, so history is never lost silently (`Entered::fell_back()`). The summary **chains**: a
  flow summary at message 1 (from an earlier `ResetWithSummary` followed by `Append` nodes) is passed
  to the summarizer as `previous` and is not part of the input slice, so a second summary extends the
  first even when the newer history is cut to whole turns; with nothing new since, the old summary is
  carried over without a summarizer call. Input messages are clipped like the window clips them
  (`SUMMARY_MESSAGE_MAX_TOKENS`, 1000), and a history with no user message is cut at assistant
  messages, so the input always respects `summary_input_tokens` (plus the latest turn).
- **Latency of `ResetWithSummary`.** The summarizer is an LLM call on the voice path: up to
  `summary_timeout` of silence with `apply`. Use `apply_results` + `resume_pending` and play
  `pending_pre_actions()` (the node's `Say`, for example "One moment please.") while it runs, set a
  shorter `FlowConfig::summary_timeout` (1-3 s), or use `Reset`/`Append` for nodes where the
  summary is not worth it. A timeout costs nothing but the summary: the history is kept.
- **`keep_last_user`.** `Reset` and `ResetWithSummary` leave `[prompt]` or `[prompt, summary]`: no
  user message, which Anthropic rejects. `Node::with_keep_last_user(true)` copies the newest user
  message of the history behind them (the example's `confirm` and `done` do), so the context is
  request-shaped. Off by default, so existing flows keep their exact contexts.
- **A reset invalidates indices.** `Reset` and a `ResetWithSummary` that summarized replace the
  whole context (`Entered::context_replaced`, `Applied::context_replaced`). Anything the runner keys
  by message index refers to messages that no longer exist: the window `Summary`
  (`covers_until` is an index; kept, the next `build_window` fails with `InvalidSummary` and every
  later request of the call would fail), the round to message-index map of the play-out book, and the play-out
  book's truncation indices. The runner must drop them when `context_replaced` is set (test
  `a_reset_invalidates_a_kept_window_summary_unless_the_runner_drops_it`). `Append` (also a summary
  that fell back to it) keeps indices; they stay valid. A flow summary and a window summary are
  otherwise independent: the flow summary is an ordinary system message at index 1, which the
  window keeps as part of the header.
- **Atomic transitions and cancellation.** The node, the generation and the context change only
  after the whole transition succeeded; the only await (the summarizer) comes before any mutation.
  Everything before it (results appended, state captured, the transition decided) is synchronous
  and recorded in `pending_transition()`, so dropping `apply` at the await loses nothing: the
  result of a SideEffect call is in the context, the decision is kept, and no tool is offered until
  it is completed or abandoned (test `a_dropped_apply_keeps_the_decision_...`).
- **Which transition wins.** For the calls of one response in issue order, the first that fires and
  names a node of the flow. Later ones are counted (`Applied::ignored_transitions`). A target outside
  the flow (only possible with `Custom`) is not an error: it is reported in `Applied::rejected`, logged,
  and the node does not change. A failed call (an `is_error` result: `timeout`, `tool_failed`,
  `unknown_outcome`, ...) never fires `Goto`, so the model can retry; `Rules` can route on it.
  **Answers that are not tool results never transition**: when `Execution::budget_exhausted` is
  set no transition is decided. An error result is routable only if the tool ran: the allow-list is
  `tool_failed`, `timeout`, `unknown_outcome` and `result_too_large` (`TOOL_RESULT_ERRORS` in
  `manager.rs`). Every other error is skipped before any `Condition` or `Custom` function sees it:
  `budget_exhausted`, `stale_turn`, `interrupted`, `duplicate_call`, `invalid_arguments`,
  `arguments_too_large`, `too_many_calls`, `unknown_tool`, `store_full`, a malformed call (`invalid
  tool call`), `missing_result` and `tool_not_available` (the flow's own), and any kind not on the
  list (a refusal the engine gains later is not routed until someone adds it; a new tool-run kind
  has to be added too). So a `Failure` or `Always` rule cannot enter an apology node, or wipe the
  context, because the model sent arguments that fail the schema, the retry budget was spent or
  the idempotency store was full: the model just retries. A call that did run and failed is
  routable, `unknown_outcome` included: a SideEffect call that timed out may still be running, and
  `Always` follows it like any failure; use `Success` or a `ResultEquals` rule after a booking.
- **Refused tools.** `screen` answers a call to a tool the current node does not offer with
  `{"error":"tool_not_available","tool":...,"detail":"... Available tools: a, b."}` (same shape as the
  engine's errors) and does not pass it to the engine, so it cannot run and spends none of the
  engine's retry budget. The model can call a tool it remembers from an earlier node; this is the
  guard. Calls the assembler could not parse go to the engine, which answers them. `Screened::merge`
  restores issue order; `apply` does it itself.
- **SideEffect tools run once.** The engine's idempotency key (session, epoch, call id) already
  guarantees it per id. The flow adds three things. A `Screened` is consumed by `apply`, so a round
  cannot be applied twice. A round screened before a transition is stale after it (`Applied::stale`:
  results of calls still in the context appended, nothing captured, no second transition; results
  whose calls a reset removed are dropped, `Applied::dropped_results`, so the context stays valid;
  a transition cannot intervene while the round's assistant message is in the context, because
  `transition_to` needs every call answered, so a stale round only arises when the runner screens
  before it pushes that message). The tool of a node that was left, or that is being left
  (`pending_transition()`), is refused. A transition lost to a late result is recoverable:
  `apply_recorded_result(call, content, is_error)` evaluates the current node's binding on a
  result that arrived after its slot was answered (the late note is already in the context) and
  makes the target pending. The fourth is an unsettled call: a SideEffect call whose result is
  `timeout`, `unknown_outcome` or `interrupted` may still be running (or have happened), and the
  model's retry would carry a fresh call id that the engine's idempotency key does not match. The
  manager remembers the tool (`unsettled_tool`), stops offering it (`tool_specs`, `allows_tool`)
  and answers a call to it with `tool_not_available` and the detail "ended without a known outcome
  ... Do not call it again". The tool is released when `apply_recorded_result` receives a real
  result of the call (a success, or a `tool_failed`: the effect did not happen) or when the node
  changes; a repeated `timeout`/`unknown_outcome` keeps it withheld and engine refusals change
  nothing. Conservative choices: any `interrupted` of a SideEffect tool counts (including the rare
  one before the tool started), and a `unknown_outcome` that never gets a late result keeps the tool
  withheld in this node for the rest of the visit (hand the caller to a checking node or a human with
  a `Rules` binding on `unknown_outcome`). The fifth is a hazard the flow had to close: `Reset` removes tool calls from the context,
  and `Context::next_tool_call_ids` counts calls in the context, so it would hand out `t<epoch>.0`
  again in the same turn, and the engine would treat a new SideEffect call as the earlier one and not
  run it. `FlowManager::next_tool_call_ids` continues after the removed calls (it remembers a count
  for the newest epoch only; an older epoch is refused by the engine as stale anyway; the count
  accumulates over several resets of one turn). **The runner and the assembler must mint ids
  through it while a flow runs**.
- **State.** `capture_args()` merges the top-level arguments of every successful call into a state
  map; `capture_result(key)` stores the result (JSON when it parses). Bounds: `MAX_STATE_ENTRIES` (64)
  keys, `MAX_STATE_KEY_BYTES` (64) per key and `MAX_STATE_VALUE_BYTES` (4096) per value; one over
  the bound is dropped and counted (`state_dropped()`), an existing key can be overwritten. A value
  dropped for its size also removes the key's earlier value, so the prompt of the node a new result
  led to never shows data of an older call as current (the placeholder stays literal). Keys of
  `capture_args` are chosen by the model: an argument whose name is a `capture_result` key of the flow
  is dropped (counted), so it cannot shadow a captured tool result. Task messages use `{key}` or `{key.sub.0}`
  placeholders; unresolved ones stay literal. Substituted text is cut at 256 characters, control
  characters become spaces, and it is never scanned again. Values come from caller speech via the
  LLM and end up in a system message, so constrain the schemas of tools that feed prompts (enums,
  `maxLength`). `Debug` of the manager prints state keys, never values.
- **Pre and post actions** are returned in `Entered`, not performed. `Say` is a canned utterance that
  is not added to the context (unlike the tool filler, which is, see TOOLS.md); `EndCall` as a
  post action means "hang up after this node's first response was played". `respond_immediately`
  false makes `Applied::respond` false: the runner ends the turn and waits for the caller.
- **Time.** Only the summarizer wait is timed, with `Clock::sleep` from the injected clock. No
  `Instant::now`. Everything else is pure and synchronous.

### Example

`flows::example::restaurant_flow(Language::En | Language::Tr, &Arc<Restaurant>)` over an in-memory
`Restaurant` (one table per date and time, some times always full) and three tools:
`check_availability` (ReadOnly), `make_reservation` (SideEffect), `end_call` (ReadOnly, `Ack`).

```text
greeting --check_availability--> confirm (ResetWithSummary, pre: Say) --make_reservation--> done (Reset, post: EndCall)
   |  free=false                    ^   |  status=unavailable (lost the race)
   '--> alternatives (Append) ------'   '--> alternatives
 end_call (global) --> farewell (Append, post: EndCall)
```

### Tests

`crates/brisvox-agent/tests/flows.rs` (57 tests; paused time, real `ToolEngine`, a scripted
`Summarizer`) calls `Context::validate()` and checks that message 0 equals `system_prompt()` after
every step. Unit tests are in `flows/render.rs`, `flows/def.rs`, `flows/manager.rs`.
- English and Turkish reservation: transitions by `Rules`, all three strategies, state in prompts,
  pre and post actions, the exact messages the summarizer received.
- Busy slot to `alternatives` (Append keeps history) and back; a booking lost to a race by a second
  session returns to `alternatives` with one booking in total; the global tool from any node.
- Strategies on a generic flow; summary fallbacks (none, error, empty, timeout: the wait is
  `summary_timeout` of simulated time); nothing to summarize; clipping and whole-turn input cut.
- Invalid transitions: unknown target rejected and nothing changes, failed tool stays, first of
  several wins, stale round, use before `start`, pending calls, foreign message 0, unknown node,
  dangling references in the definition.
- Refused tools (never run, issue order kept, unparseable calls still reach the engine).
- SideEffect once: a transitioning `make_reservation` runs once, a replay of its id at the engine
  returns the recorded result, a call in the left node is refused, and the id hazard above (a second
  SideEffect call after a `Reset` in the same turn does run).
- State bounds, prompt sanitising, `refresh_prompt`, and the window keeping the node prompt after an
  `Append` transition over 40 turns.
- Review fixes: `stale_turn` (real engine) and `budget_exhausted` answers never fire `Failure`,
  `Always` or `Custom`; hand-built `Execution`s with too few results (`missing_result`, issue order,
  refusals interleaved), too many, `late_notes` (after the results; dropped by a Reset); a stale
  round whose calls a Reset or summary removed drops its results; `apply` dropped mid-summary
  (decision kept, no tool offered, `resume_pending`, next-round and `abandon_pending` paths, one
  booking); `apply_results` returns the pre actions before the summarizer is asked;
  `apply_recorded_result`; `build_window` after `Append` (kept summary valid) and after `Reset*`
  (kept summary `InvalidSummary`, none fine); `keep_last_user`; a second summary chaining the
  first, carried over when nothing is new; summarizer input clipped; key bounds and result-key
  collisions.
- Review fixes (round 2, each test fails without its fix): a real `invalid_arguments` answer (and
  a table of `arguments_too_large`, `too_many_calls`, `unknown_tool`, `store_full`, malformed and
  unknown kinds) never fires `Failure`, `Always`, `Custom` or a SideEffect binding while the
  tool-run kinds do; an oversized capture removes the key's older value (prompt keeps the
  placeholder); a timed-out SideEffect tool (real engine, 5 s tool, 1 s timeout) is withheld,
  refused on retry (one run), and released by a recorded late result, a real failure or a node
  change; five SideEffect calls in one turn across Reset / ResetWithSummary / Reset / Reset get
  ids `t1.0..t1.4` (all ran), a new epoch restarts at zero, and an older-epoch leftover behind a
  newer call does not move the offset; the `budget_exhausted` flag stops a transition that a
  genuine success asks for, and without the flag the same success transitions.

### Known gaps

- The summary input covers only the newest `summary_input_tokens` of history (the earlier flow
  summary is chained as `previous`). A summary the window made for the runner is not in the flow
  summary's input: the flow summarizes what is in the context, and a reset makes that window
  summary invalid anyway (see "A reset invalidates indices").
- `Reset`/`ResetWithSummary` without `keep_last_user` yield a context with no user message.
  Providers that need one (to verify: Anthropic) need that option or a converter placeholder.
- A SideEffect call that timed out (`timeout`, `unknown_outcome`, `interrupted`) keeps the node, but
  its tool is withheld (`unsettled_tool`) until `apply_recorded_result` gets the real result or the
  node changes, so a retry with a new id cannot book twice. The flow cannot tell whether the first
  call ever finishes: if no late result comes (`unknown_outcome`), the tool stays withheld for the
  visit. Route `unknown_outcome` to a checking node with a `Rules` binding. Only the runner
 can feed late results to `apply_recorded_result`; until it does, a timed-out booking
  is never retried by the flow. A failed call that ran (`tool_failed`) does not withhold the tool.
- Which timestamp or runner event triggers `resume_pending` after a cancelled `apply` is the
  runner's job; the flow only guarantees that nothing runs in the meantime.
- A node whose context still holds tool calls from an earlier node but offers no tools (the `done`
  node) may be rejected by providers that require tool definitions next to tool-use blocks (to
  verify: Anthropic); `Reset`/`ResetWithSummary` nodes avoid it by dropping the calls.
- A refusal does not spend the engine's malformed-call budget; a model that keeps calling an
  unavailable tool is bounded only by the runner's `max_tool_rounds`.
- Pipecat's `function` actions (a flow-invoked handler) and dynamic node factories are not
  provided; use `Action::Custom`, `transition_to` and `set_state` from application code.

## Turn-taking

Code: `brisvox_agent::turn` (`TurnMachine`, `TurnPolicy`, `TurnState`, `TurnInput`, `TurnAction`).
A pure, synchronous machine: every input carries its time (`tokio::time::Instant`, a reading of the
injected clock); it does no IO, reads no clock and owns no task. `handle(input, &mut out)` appends
the actions the caller must carry out:

| Action | Caller does |
|---|---|
| `StartUserTurn{epoch}` | emit `Control::UserTurnStarted` |
| `RequestAnalysis{id}` | run Smart Turn on the inference pool; feed `Verdict{request: id, ..}` back (a late verdict is ignored, not cancelled) |
| `EndUserTurn{epoch, reason}` | emit `Control::UserTurnEnded`; start the response, except for `EndReason::Hangup` |
| `Interrupt{new_epoch}` | emit `System::Interrupt`; always followed by `StartUserTurn` in the same epoch |
| `ArmTimer{deadline}` / `CancelTimer` | one timer slot: `ArmTimer` replaces any pending deadline; deliver `Tick{now}` at (or after) it |

### State diagram

```
                 VadStart (first turn: epoch 0; each later user turn: epoch + 1)
   +------+ ------------------------------------------> +--------------+
   | Idle |                                             | UserSpeaking |
   +------+ <-- BotAudioEnded (current epoch) --+       +--------------+
      ^                                         |        |          ^
      |                                         |  VadStop          |  VadStart: resume the
      |                                         |  (Adaptive: a     |  same turn (same epoch),
      |                                         |   RequestAnalysis)|  cancel the timer
      |                                         |        v          |
      |                                         |   +------------+  |
      |                                         |   | UserPaused |--+
      |                                         |   +------------+
      |                                         |        | Verdict complete (ends at once, or
      |                                         |        | waits for the STT final), Tick at the
      |                                         |        | deadline, or a punctuated final
      |                                         |        v                        (see policies)
      |                                         |   +------------------+
      |                                         +---| AwaitingResponse |   <- EndUserTurn
      |                                             +------------------+
      |                                                |            ^
      |                                  BotAudioStarted|            | VadStart = barge-in:
      |                                  (current epoch)|            | Interrupt{new} +
      |                                                v            | StartUserTurn{new}
      |   BotAudioEnded (current epoch)         +-------------+     |
      +-----------------------------------------|  BotSpeaking |----+
                                                +-------------+

   Hangup (any state) -> Closed (terminal: every input is ignored; an open user turn is ended
   first with EndUserTurn{reason: Hangup}; the timer is cancelled)
   UserText (a typed message) in Idle / AwaitingResponse / BotSpeaking -> AwaitingResponse:
   [Interrupt{new} when the bot is busy] + StartUserTurn{new} + EndUserTurn{new, reason: Text};
   with a pending barge-in candidate the candidate is confirmed instead (-> UserSpeaking, the
   caller adds the text to that turn); ignored in UserSpeaking / UserPaused / Closed
```

Transitions are exhaustive: `crates/brisvox-agent/tests/turn.rs` checks every (state, input) pair
(6 states x 10 inputs = 60) for each policy, asserting actions, next state and whether the input
was counted as ignored. A second table keys the pause (`UserPaused`) by its analysis sub-state
(NotRequested, Pending, Incomplete, Complete), with and without an STT final, punctuated and not,
against the inputs that end or shorten it, including the deadline cap.

- `Idle`: `VadStart` starts a turn. `BotAudioEnded` of the current epoch is ignored. (`Idle`
  can also hold a *detached* barge-in candidate, see "Interruption policy": the bot's audio
  ended while the user was speaking over it and the policy has not decided yet.)
- `UserSpeaking`: `VadStop` opens a pause (`UserPaused`). `SttInterim` is accepted with no effect.
  `SttFinal` answers an owed stop (see the caller contract); when no stop is owed it is ignored
  and counted. `VadStart` again is a duplicate and ignored.
- `UserPaused`: the candidate end of turn. Carries the VAD stop time, the Smart Turn progress
  (`NotRequested`, `Pending(id)`, `Incomplete`, `Complete`) and the STT final of this pause. The
  one armed deadline is computed from this data. `VadStart` resumes the same turn and drops the
  pending analysis (its verdict is then stale).
- `AwaitingResponse`: the user turn ended; waits for the bot. `BotAudioStarted` of the current
  epoch moves to `BotSpeaking`. `BotAudioEnded` of the current epoch means the response produced
  no audio and returns to `Idle`. `VadStart` is a barge-in candidate (see "Interruption policy").
  A final that answers an earlier stop is consumed with no effect.
- `BotSpeaking`: `BotAudioStarted` of the current epoch again is accepted. With the echo guard
  off it has no effect; with it on it restarts the echo window only for audio that resumes after
  a real gap (at least `echo_guard` after the previous `BotAudioStarted`); closer, repeated or
  older reports change nothing (see "Echo window").
  `BotAudioEnded` of the current epoch returns to `Idle`. `VadStart` is a barge-in
  candidate.
- Stale audio: `BotAudioStarted` and `BotAudioEnded` of any other epoch are ignored and counted
  (tested in `AwaitingResponse` and `BotSpeaking` at epoch 1, for every policy: state unchanged,
  no actions, ignored count +1; without the epoch guard a late end of the interrupted response
  would return the machine to `Idle` and the next `VadStart` would emit no `Interrupt`).
- `Closed` ignores every input, including an `SttFinal` still owed by a stop that was open when
  the call hung up (tested after hang-ups from every state).
- Barge-in (from `AwaitingResponse` or `BotSpeaking`): epoch + 1, `Interrupt{new_epoch}`, then
  `StartUserTurn{new_epoch}`, state `UserSpeaking`. There is no separate "interrupted" state: no
  state exists in which no user turn is open. Whether a `VadStart` is a barge-in at all is
  decided by the interruption policy (next section); with the default (disabled) policy
  it is, at once.
- `Closed`: terminal. `Hangup` from `UserSpeaking` or `UserPaused` emits
  `EndUserTurn{reason: Hangup}` so that every `StartUserTurn` is paired with an end; the caller
  emits `UserTurnEnded` and starts no response. From any other state it emits nothing.

### Interruption policy

Code: `brisvox_agent::interrupt` (`InterruptionPolicy`, `BackchannelSet`, `Observation`,
`Assessment`), `TurnMachine::with_interruption(turn_policy, interruption)`. `TurnMachine::new`
uses `InterruptionPolicy::disabled()`, which confirms every candidate at once: the plain turn-machine behaviour,
all existing tests and the sim unchanged.

**Representation.** A `VadStart` in `AwaitingResponse` or `BotSpeaking` creates a *barge-in
candidate* (`TurnMachine::barge_in_candidate()`), a field next to the state, not a `TurnState`
variant. While it is pending: `state()` is unchanged, no user turn is open, the epoch is
unchanged, and nothing but the recheck timer has been emitted. So a vetoed barge-in cannot bump
the epoch, emit `Interrupt`, or emit `UserTurnStarted`, and the stale-epoch rules for bot audio
keep working untouched. (A new `TurnState` variant would also have changed the exhaustive
transition tables.) The candidate stores when VAD started, the largest word count of the interims
seen, and what the latest interim looks like (`TextKind`: none, speech, backchannel).

When the bot's audio ends while a candidate is pending (`BotAudioEnded` of the current epoch),
the candidate is *detached*: the state becomes `Idle`, the bot is silent (the echo window no
longer applies) and the candidate stays pending. The other guards keep judging it, so a
"mm-hmm" or a cough over the last word of a sentence does not become a user turn.

**Resolution.** The machine asks the policy (`assess(Observation)`) on the `VadStart`, every
`SttInterim`, every `Tick` of the recheck timer, `BotAudioStarted` (the echo window starts) and
the `VadStop`:

| Event | Result |
|---|---|
| policy confirms | `Interrupt{new}` + `StartUserTurn{new}` (as without a policy), candidate cleared |
| `VadStop`, policy still blocks (judged with `assess_at_stop`, see below) | candidate dropped (`InterruptionStats::vetoed(reason)`), no actions besides `CancelTimer` when a timer was armed; the stop counts as an owed STT final so later finals stay aligned |
| `VadStop`, policy confirms (the clock passed the threshold, tick not delivered yet) | barge-in, then the ordinary pause of the new turn |
| `BotAudioEnded` of the current epoch | the candidate is judged again with the bot silent (no echo window). Confirmed: the user is speaking into silence, so `StartUserTurn{epoch+1}` without `Interrupt` (`promoted`). Still blocked: it stays pending in `Idle` (detached) and is decided by the next interim, tick or `VadStop` like any candidate; a veto at the stop starts no turn and does not bump the epoch |
| `Hangup` | candidate dropped, timer cancelled, no `EndUserTurn` (no turn was open) |

A second `VadStart` while a candidate is pending (also a detached one) is a duplicate: ignored
and counted. An `SttInterim` with no candidate in `Idle` / `AwaitingResponse` / `BotSpeaking`
stays ignored and counted. A `Tick` while a candidate is pending is always read as the recheck
timer (consumed, the candidate judged, the timer re-armed if needed); a tick that carries a time
before the candidate began is judged as of the start.
Every candidate ends exactly once as confirmed, promoted or vetoed (the invariant test checks
`candidates == confirmed + promoted + vetoed + pending (+ dropped by hangup)`).

**Line-dependent vetoes (hooks).** Three things can hold a candidate that the table below does not
cover, all fed in from outside because the machine stays pure: `TurnMachine::set_echo(bool)` marks the
candidate's words as the bot's own voice coming back (the turn driver sets it with every interim when
`interruption.echo_text_match` is on, from the text the bot has just sent to TTS: `PlayoutBook::recent_sent_text`,
`brisvox_agent::is_bot_echo`, built on `echoes_bot`, which never calls a short command ("stop", "wait") an echo;
the veto is `EchoGuard` and the next interim can clear it);
`TurnMachine::set_interruption` replaces the policy (used by `InterruptionPolicy::adapted_to_noise`, a
noise-floor-adapted tightening that measured no benefit and is not wired into the engine); and the level
gate of the VAD (`[vad] level_gate_db`) works before the machine, at the speech start. **The hook for a
speaker check** (not built; the research is in `docs/providers/noise-handling-options.md`, section 3) is the
same as the echo's: a candidate that is "a different speaker than the caller" is one more reason to hold it,
set from outside as `set_echo` is. It would need (a) the audio of the candidate, the first 0.5 to 1.5 s from
its start, which the turn driver has in the Smart Turn ring (the enhanced audio when a denoiser is on); (b) a
speaker embedding of the caller, taken from their first utterance on the same call and the same channel (a
floor of about 3 s of speech; no veto before it), computed on the inference pool; (c) a decision rule that
fails open (only a clearly different speaker vetoes) and a deadline (a candidate held for the embedding costs
the real barge-in that long: the embedding needs about a second of audio, against the 300 ms of `min_speech`).
Whether a model of it generalises to Turkish and to 8 kHz mu-law is not known (the weights' license and the
training data are the open points of the research note).

**Guards** (all must pass; a guard that is off never blocks):

| Guard | Field | Blocks while |
|---|---|---|
| Backchannel | `backchannels`, `BackchannelSet`, `backchannel_max_speech` | the latest interim is only backchannel phrases and VAD has run for less than `backchannel_max_speech` (zero: no cap). The cap keeps a stale interim (STT lag or loss) from swallowing a long real utterance; a real backchannel is short |
| Echo guard | `echo_guard`, `echo_min_words` | bot audio younger than `echo_guard` and fewer than `echo_min_words` (at least 1) interim words: VAD alone cannot interrupt in the first N ms |
| Minimum speech | `min_speech`, `vad_hangover` | VAD active for less than `min_speech`. Live (a tick or an interim before the stop) the comparison uses the clock time minus `vad_hangover`, and the recheck tick is armed at `min_speech + vad_hangover` (see "VAD hangover") |
| Minimum words | `min_words` | fewer interim words than `min_words` |
| Transcript wait | `backchannel_wait` | backchannels on, no interim yet, VAD active less than the wait (fail open: after it, a silent STT does not stop a real interruption). Zero means the 350 ms default (`DEFAULT_BACKCHANNEL_WAIT`): with no wait the backchannel guard could never act, since a candidate is judged on its VAD start, before any interim exists |

`min_speech` and `min_words` combine with `Confirm::All` (default: both) or `Confirm::Any`.
Time-driven guards give the machine a recheck time (`recheck_in`): the earliest of the
min-speech end, the echo window end, the transcript wait end and the backchannel cap; guards
that only words can lift arm no timer. Interims are cumulative per utterance: the word count is the maximum seen
(a restarted interim cannot lower it), while the backchannel test looks at the latest interim
only (a partial "ye" is not a backchannel; STT providers emit whole words, which is why the
lists hold whole words).

**Backchannel matching.** Text is lower-cased ("İ" becomes "i"), split on every non-alphanumeric
character ("uh-huh" is "uh", "huh") and must be fully covered by a sequence of phrases of any
configured set (at most 16 tokens: tokenising stops after the 17th, so the cost does not grow
with the length of a long cumulative interim). Turkish dotless "ı" stays distinct
from "i". Defaults (`BackchannelSet::english()` / `turkish()`) are **guesses, not tuned**:
English yeah, yep, uh-huh, mm-hmm, mhm, hmm, okay, right, alright, sure, "i see", "got it",
cool, oh, ah, uh, um; Turkish evet, hı-hı, hı, he, hm, mhm, tamam, aynen, anladım, anlıyorum,
peki, tabii, "öyle mi", ya. Answers ("yes", "no", "hayır") are deliberately not in the lists.
`InterruptionPolicy::recommended()` (300 ms min speech, 100 ms VAD hangover, 400 ms echo
window with one word, both languages, 350 ms transcript wait, 1.5 s backchannel cap) is also
an untuned starting point.

**VAD hangover.** A VAD reports a stop only after its silence hangover (`min_silence`: 100 ms in
`brisvox-inference`'s defaults, 300 ms in the simulator's energy VAD) and backdates it to the end
of the speech. A 220 ms cough against `min_speech = 300 ms` would therefore still look "active"
at the tick armed for `started + 300 ms`, before its stop (delivered at about `started + 320 ms`)
arrives, and the clock path would confirm a burst the stop path (which uses the backdated time)
vetoes. `InterruptionPolicy::vad_hangover` closes that: while a candidate is judged live,
`min_speech` is compared with `elapsed - vad_hangover` and the recheck is armed at
`min_speech + vad_hangover`; at the stop the backdated duration is exact and nothing is
subtracted. It defaults to zero (stops assumed undelayed); `recommended()` sets 100 ms
(`DEFAULT_VAD_HANGOVER`); set it to the real VAD's `min_silence`. The price is that a genuinely
long utterance is confirmed `vad_hangover` later than `min_speech` alone would say (conservative
choice: a missed 100 ms of reaction rather than cutting the bot off for a cough).

**At the VAD stop** the machine uses `assess_at_stop`: when the latest interim is speech with at
least one word (or a backchannel past its cap), the `min_speech` duration is waived, because the
utterance is over and STT heard a word, so a short real "stop" or "no" at 250 ms is not lost.
`min_words`, the backchannel guard and the echo guard still apply as configured (an echoed single
word inside the window with `echo_min_words >= 2` is still vetoed at the stop; tested in
`the_echo_guard_still_applies_at_the_vad_stop_when_a_word_waives_min_speech` and the pure
`assess_at_stop_table`).

**Echo window.** It counts from the first `BotAudioStarted` of the response, or from the latest
one that came at least `echo_guard` after the previous `BotAudioStarted` of the epoch (a real
gap), so a new round of bot audio after a gap (text, tool call, text) is guarded again. The
machine cannot tell a burst from a chunk by itself, so it uses the gap: a report is compared with
the previous *report* (not with the window start), and a report that is not later than the
previous one is ignored. Per-chunk reporting (every 20 ms) therefore never restarts the window and
cannot hold it open for the whole response; one report per burst restarts it when the burst
begins at least `echo_guard` after the previous report. Two bursts closer together than
`echo_guard` (measured between their reports) share one window, the conservative side: the user
can interrupt sooner, not later. Tests: `per_chunk_bot_audio_reports_do_not_keep_the_echo_window_open`,
`a_repeated_or_late_bot_audio_report_never_moves_the_echo_window_backwards`,
`the_echo_window_restarts_only_after_a_gap_of_the_guard_since_the_last_report` and the
simulator-level `per_chunk_bot_audio_reports_cannot_block_a_wordless_interruption`.

**Trade-offs and known limits.**

- A candidate still blocked at its `VadStop` is dropped for good: no `Interrupt`, no user turn,
  nothing for the LLM, only the `vetoed(reason)` counter. That is the cost of the guards. It
  covers a backchannel or a cough, but also a very short real utterance that STT did not
  transcribe by the stop (with `recommended()`, a 250 ms "stop" without an interim), and a
  one-word interruption when `min_words >= 2`. Callers that cannot afford it keep `min_speech`
  low and deliver interims promptly.
- The echo guard is lifted by any interim with `echo_min_words` words. When the VAD fires on the
  bot's own echo, STT transcribes that echo, so the words confirm the barge-in from the echo
  itself. The guard only stops VAD-only (wordless) echo blips. Comparing the interim with the
  bot's currently played text would close this gap and is a possible follow-up, not part of the interruption policy.
- After `BotAudioEnded` the echo window no longer applies to a detached candidate. With an
  echo-only policy (no `min_speech`, no backchannels) an echo tail overlapping the end of the
  bot's audio therefore becomes a user turn once the bot is silent; combine the echo guard with
  `min_speech` (as `recommended()` does) to cover that.
- A detached candidate that is vetoed at its stop (for example a "mm-hmm" over the last word of
  an answer) gets no reply, like any other vetoed candidate.
- The guard hook is `InterruptionPolicy::assess` / `assess_at_stop` inside the machine; the
  driver feeds interims and ticks and needs no separate hook.

**Caller contract additions.** Deliver `SttInterim` text promptly (before the `VadStop` event):
a short real interruption with no interim by the stop is judged on VAD alone. `BotAudioStarted`
must carry the time the audio reaches the user (the echo window counts from it). Set
`vad_hangover` to the VAD's `min_silence`.

Tests: `crates/brisvox-agent/tests/interruption.rs` (table rows for each guard in both
languages, candidate life cycle, the pure policy including `assess_at_stop`, randomized
invariants) and `crates/brisvox-agent/tests/interruption_sim.rs` (simulator level, see below).

**Simulator-level tests.** `interruption_sim.rs` plays scripted calls built from
`brisvox_sim::scenario::Scenario` (the caller's speech) with the simulator energy VAD's
`VadConfig` timing (40 ms start, 300 ms stop hangover, backdated stop) through a `TurnMachine`
behind a thin deterministic driver (virtual time; STT interims every 200 ms, a final after each
stop; every ended turn answered by a scripted bot; the stale bot audio flushed at an
`Interrupt`). It covers each guard (backchannels in both languages, echo burst, words inside the
echo window, cough, minimum words, short and long real interruptions, per-chunk bot-audio
reports) and checks the s-suite style invariants on every run (epochs only grow, `Interrupt`
followed by its `StartUserTurn`, starts equal ends, no stale bot audio delivered, no leftover
timer or candidate). The reference pipeline's `TurnManager` (scenarios s01..s11) does not use
`TurnMachine` yet; running s02/s10 with an `InterruptionPolicy` through the real pipeline is part
of wiring the machine in, not of the interruption policy.

### Caller contract

- VAD: `VadStart` at speech start, `VadStop` at the backdated stop (the start of the silence).
- STT: exactly one `SttFinal` per `VadStop`, in the same order. The STT stage finalizes on every
  VAD stop (as the reference `TurnManager` does, `pending_finals`), and the final's text is the
  transcript of that utterance. A final answers the oldest VAD stop not yet answered. Only the
  final that answers the current pause's stop can shorten or end that pause; a final with no
  unanswered stop is ignored and counted. A VAD stop whose final never arrives misaligns every
  later final by one: the fallback deadline still ends the turn, but the STT signals are then
  unreliable (see Known gaps).
- Smart Turn: each `Verdict` echoes the `AnalysisId` of its `RequestAnalysis`.
- Timers: one timer slot. Deliver `Tick{now}` at (or after) the armed deadline; a `Tick` for a
  timer that was cancelled is ignored.
- Bot audio: `BotAudioStarted{epoch}` at the first audio of the response, and again when audio
  resumes after a gap (a new TTS round). With the echo guard on, a repeat restarts the echo
  window only when it comes at least `echo_guard` after the previous report, so per-chunk reports
  are harmless (they never restart it); `BotAudioEnded{epoch}` when the response's audio ends. For a response that makes no
  LLM request (an empty transcript, for example) send `BotAudioEnded` straight after
  `EndUserTurn`. Otherwise the machine waits in `AwaitingResponse`, and the next `VadStart` is
  reported as a barge-in against nothing.
- Hangup: `Hangup` once, when the session ends.

### Policies

`TurnPolicy::FixedSilence { silence }`: the turn ends when VAD has been silent for `silence` after
the VAD stop. The VAD stop time is backdated by the VAD's minimum silence (`min_silence`), so the
silence is counted from its start. Deadline = VAD stop + `silence`; `Tick` at the deadline ends the
turn with `EndReason::FixedSilence`. STT finals are consumed but do not affect the deadline.

`TurnPolicy::Adaptive { fallback_silence, require_final, final_grace }` (Smart Turn + STT):
- Every `VadStop` requests one analysis (`RequestAnalysis`, a fresh id).
- Fallback deadline = VAD stop + `fallback_silence` (default 3 s, Pipecat's `stop_secs`). It is
  always armed while the pause is open and ends the turn with `EndReason::Fallback` when reached.
- Complete verdict, `require_final = false`: the turn ends at once (`EndReason::Verdict`).
- Complete verdict, `require_final = true`: the turn waits for the STT final of this pause (ends on
  it with `Verdict`). The deadline becomes verdict time + `final_grace` (default 800 ms) if that is
  earlier than the fallback; reaching it ends the turn with `EndReason::FinalGrace`. The end
  reason is `FinalGrace` as well when the fallback comes first.
- Incomplete verdict: the fallback still applies. The STT final of this pause, when its text ends
  with sentence-final punctuation (`. ? ! …` as the last non-whitespace character), shortens the
  wait: the deadline becomes final time + `final_grace` when that is earlier than the fallback,
  and it ends with `EndReason::SttPunctuation`. The rule holds whichever of the two arrives first
  (the final during the analysis or after the verdict).
- Only one final per pause counts: the one answering this pause's VAD stop. A second final in the
  same pause is ignored and counted, so it never moves the deadline. A final of an earlier pause
  (the user resumed in between) is consumed and never attributed to the current pause.
- A verdict whose `request` is not the pending analysis of the current pause is stale and ignored.
  So are verdicts after a resume, a second verdict for the same analysis, and verdicts in any
  state without a pause.
- An unpunctuated final does not shorten the wait. It still answers the pause, which matters for
  `require_final`.
- The deadline never moves later than the fallback: a late verdict or final whose grace would end
  after the fallback leaves the fallback deadline in place.

Defaults: `DEFAULT_FALLBACK_SILENCE` = 3 s, `DEFAULT_FINAL_GRACE` = 800 ms, `require_final = false`
(`TurnPolicy::adaptive()`).

### Epochs

The first user turn runs in `TurnEpoch::ZERO`. Every later user turn start (from `Idle`) advances
the epoch by one, as the reference `TurnManager` does. A barge-in also advances it, and
emits `Interrupt`. A resumed pause keeps its epoch. Actions carry the epoch they belong to;
`EndUserTurn` and `StartUserTurn` always name the current epoch.

### Timers and ignored inputs

- At most one deadline is pending at a time: `ArmTimer` replaces the previous one, `CancelTimer`
  clears it. The machine emits nothing when the desired deadline is unchanged. `Tick` consumes the
  timer that fired; a `Tick` earlier than the deadline re-arms it, and a `Tick` outside a pause or a pending barge-in candidate is
  ignored (a stale timer after a resume or hang-up).
- An input with no legal transition in its state leaves the state unchanged and appends nothing.
  It is counted per input kind (`TurnStats::ignored(kind)`, `total_ignored()`) and logged at
  debug level. Accepted inputs with no effect (an interim during speech, a final answering an
  earlier stop, a repeated `BotAudioStarted`) are not counted, so the counters show only inputs
  the caller sent out of step.
- No input can panic: deadlines use checked arithmetic (a deadline beyond the clock range is
  clamped to about 100 years; if even that does not fit, to 10 years, 1 day or 1 hour, and only
  at the very end of the clock's range to the input time itself). Tests drive `Duration::MAX`
  policies through every deadline kind (`huge_durations_clamp_instead_of_panicking`).
- A `Tick` strictly later than the armed deadline ends the pause exactly like one at the
  deadline (real timers fire late): tests cover `deadline + 1 ms` and `deadline + 5 s` for every
  end reason. The late-tick and stale-epoch properties are also checked as oracles in the
  randomized invariant test.
- While a barge-in candidate is pending the single timer slot holds its recheck deadline
  (`BargeInCandidate::recheck_at`); at most one of pause and candidate exists at a time.

### Known gaps

- `AwaitingResponse` has no deadline. A response that never starts leaves the machine waiting
  until the caller delivers `BotAudioEnded` or a barge-in. The caller must bound the LLM/TTS
  wait; a response deadline inside the machine is a follow-up.
- The machine does not cancel an analysis already running on the pool; its verdict is dropped.
- A VAD stop whose STT final never arrives leaves the finals one stop behind for the rest of the
  machine's life (the caller contract above). The fallback still ends every pause. Recovering the
  alignment needs an STT-side correlation id; that is a follow-up.

## Played-text truncation

When the caller barges in, the LLM context must hold only what was played, not the whole response
that was synthesized (the engineering rule: after an interruption, the LLM context contains ONLY what was
actually played). `brisvox_agent::played` decides that; `brisvox_agent::played::apply_truncation`
writes it into a `Context`.

**Inputs, per assistant utterance** (`PlayedText`, one per TTS context):

- The text sent to TTS, in order, as one or more segments (`push_sent`). It must be the response
  text unchanged; segments are concatenated verbatim, so a word split across two segments still
  maps.
- The word timestamps the TTS reported (`push_word(word, start)`), `start` being the offset from
  the utterance's first audio sample. Carrier in the frame model: `DataFrame::Word(Box<WordTimestamp>)`
  (`brisvox_core::frame`; `Frame` stays at most 64 bytes, checked at compile time). Timestamps may
  arrive late or out of order; the mapping is computed when `heard` is called.
- The playback position: the duration of the utterance's audio that had played at the barge-in
  instant, passed to `heard(played, &PlayedConfig)`. It is a clock reading, not a count of released
  chunks: a chunk in flight at the barge-in counts only the part that had played by then. The sim
  computes it from a per-chunk playout log (`Playout::position(at)`, `brisvox_sim::reference`).

**Rules** (`crates/brisvox-agent/src/played.rs`, module docs).

- A word is heard when playback has passed its start (`start < played`).
  `PlayedConfig::min_word_played` raises the bar (default zero: a word counts once its first sample
  played).
- Words are placed in the sent text in start order. Each is searched forward from the end of the
  previous located word, so repeated words map to successive occurrences. A located word starts and
  ends on word boundaries (`ha` does not match inside `haha`); its core is matched (leading and
  trailing non-alphanumerics dropped; a word with no alphanumerics, such as an emoji, whole).
- A word that is only punctuation (`.`, `!`, `…`) is placed at the next non-space position if the
  text has it there, and is otherwise zero-width, so timed punctuation marks never move the cut.
- The heard text is the sent text up to the end of the last heard word, plus the punctuation that
  closes it (`dünya!`, `"Merhaba!"`), with everything before it verbatim. Opening brackets and
  quotes (`(`, a quote before a letter, `“`, `¡`) are not kept; dashes are not kept. Offsets are char
  boundaries, so Turkish letters and emoji are safe.
- **Gaps.** If a timed word skips text that no timestamp covers (the TTS lost a chunk, e.g. after a
  disconnect), the heard text stops there, since the caller may not have heard the lost text. Counted
  in `Truncation::gaps`.
- **Rewritten words** (the provider wrote "bin" for "1000") are counted in
  `Truncation::unmatched_words`. Their text is known only when isolated: one such word, between two
  timed words (or at the end), whose untimed text is exactly one whitespace token. That token is its
  text. Any other unlocatable word (a run of rewrites, a rewrite that expands to several words such
  as "3.5" spoken as "üç nokta beş", or one whose text is more than one token) has unknown text. The
  heard text stops before it, so it never extends past text that is not provably heard. For
  "Oran 3.5 kg ağır" with timings through "kilogram" at 400 ms and playback at 250 ms, the heard
  text is "Oran": the caller heard "3.5" as "üç nokta", but nothing in the timings says which
  tokens those words cover, so "3.5" is not kept.
- **Invariant.** The heard text is always a prefix of what was heard. Where the timings resolve the
  text exactly, it is exactly what was heard; where they cannot, it is shorter, never longer.

**Fallback without timestamps.** When no word timestamp arrived for the utterance, the heard text is
`played x chars_per_second` (default 14 chars/s, `DEFAULT_CHARS_PER_SECOND`), cut back to a
whitespace boundary (a mid-word cut backs up to the previous space). The result has
`Truncation::estimated == true`. `TruncationStats::record` counts truncations, estimates, unmatched
words and gaps. The reference `LlmStage` records every truncation into its `TruncationStats`
(`SessionProbes::truncation_stats`), so an estimate reaches metrics; the sim test
`a_barge_in_is_counted_and_a_provider_without_timestamps_is_flagged_as_estimated` checks it
(`MockTts::without_word_timings`). An estimate is only made for a reply that was cut: never for
one that played out in full (see below).

**Applying to the context** (`apply_truncation(&mut Context, index, &Truncation)`). The index is the
utterance's own assistant message, not the last assistant message: the LLM may append messages while
the TTS is still ahead of playback, and the cut must hit the message that was played.

- Heard text equal to the message, non-empty: unchanged.
- Nothing heard and no tool calls (including an empty placeholder message): the message is removed,
  since an empty assistant message is invalid.
- Nothing heard but tool calls: the text is emptied and the calls kept. The model issued them and
  SideEffect tools may already have run (`docs/TOOLS.md`), so the call history stays as issued.
- A heard prefix: the text is replaced. `provider_data` is dropped for a message without tool calls
  (it may embed the complete text; the adapter re-derives it) and kept for a message with tool calls
  (it may be a signature that must be echoed with them). Adapters must verify that the kept
  data does not carry the full text back to the model; this is not yet checked by any adapter.
- Heard text not a prefix of the message, or the index not an assistant message: an error
  (`TruncateError`), context untouched.

**Frame purge.** `DataFrame::Word` is exempt from the epoch purge (`Frame::is_stale`), like tool
results. When an interrupt arrives, word timestamps of the interrupted turn may describe audio that
already played; dropping them would under-report what was heard. Consumers filter words by their own
epoch. This is a design choice with a test (`stale_purge_spares_system_frames_and_tool_results`).

**Timing contract.** Word timestamps of an utterance must reach the tracker before the audio they
cover is released (the sim's TTS reports a word at or before its first chunk). `apply_truncation` is
one-shot per utterance: the sim applies the cut at the next response or at stop. A word that reaches
the tracker after its cut is applied is lost, so the context under-reports what was heard.

**Sim (reference pipeline).** The `LlmStage` keeps the conversation as an agent `Context` and cuts
the interrupted response at the playback position of the barge-in instant (`Cut`), through the
production `apply_truncation`. The playout log is per chunk (`Playout`), so the position at an
instant is exact to the chunk's partial play.

**A fully played reply is not cut.** `heard()` only knows a position, so on its own it would cut a
reply that played out to the end: the rewrite rule stops at the first unresolvable run ("3.5 kg"
spoken as four words keeps only the text before it) and the rate estimate caps the text at
`played x 14` chars (a 320-char reply spoken at 16 chars/s would lose its last 40). The truncation
is for what was *not* heard, so the caller of `heard()` must say when everything was: the sim's
`Playout` records that the TTS flushed the turn (`TtsEvent::Flushed`), that no text of the turn was
lost on the way (a send the TTS stage could not make, or a TTS session that died with the turn's
text in flight and was replaced: a later flush says nothing about the lost text; both set by the
`TtsStage` through `with_play_log`) and that the pacer saw the turn's end after its last chunk had
played out (`Playout::is_complete`). When the turn is complete and the cut position has reached
all the audio released, the message stays as sent (minus trailing whitespace, which is not a word)
and no truncation is recorded. A turn that was interrupted, whose synthesis failed or lost text,
or that was cut before its end is cut as before (the chaos test for TTS disconnects found the lost
text case). The production engine must pass the same fact (utterance fully played, no
text lost) instead of a position alone.

**Tests.**

- Unit tests in `played.rs`: punctuation and spacing, word boundaries, repeated words, Turkish and
  emoji, words split across TTS segments, late and out-of-order timestamps, gaps, rewrites (isolated,
  expanded, consecutive, followed by untimed text), timed punctuation marks, opening and closing
  quotes and brackets, the minimum-played rule, the estimate (char counting, whole-word cut, flagged),
  stats, the context rules above, the utterance's own message index, and `provider_data` handling.
- A seeded randomized test (400 cases) generates replies with attached punctuation, timed, rewritten
  and untimed tokens, timed punctuation marks, random segment cuts, shuffled timestamp arrival and a
  random playback position. It asserts that the heard text is a prefix of the expected text and
  equal to it wherever the timings resolve exactly, and that the context stays valid.
- `Playout` (`crates/brisvox-sim/src/reference.rs`, unit tests): hand-computed positions at chunk
  starts, ends, mid-chunk, across an underrun gap, before the first chunk and after the end; the
  completeness flags. The sim oracle below does not use `Playout`; it recomputes the position from
  the audio frames the transport received and asserts that the playout log agrees.
- `LlmStage` (same module, unit tests): a fully played reply with a rewritten number ("3.5 kg") or
  without word timestamps (320 chars at 16 chars/s) keeps its whole text and records no
  truncation; the same reply without a clean flush, or with the cut before its end, is cut.
- Sim (`crates/brisvox-sim/tests/played_text.rs`): 40 seeded barge-ins through the reference pipeline
  are compared char-exact with the answer's own text up to the last word that started before the
  barge-in position. At least 8 *interrupted* calls have the barge-in inside a 40 ms chunk (the
  counter ignores uninterrupted answers, whose total length is not a multiple of the chunk). A
  deterministic case (TTS at 70 ms/char) aims the barge-in 340 ms into the audio, where the word
  "is" starts at 350 ms inside the in-flight chunk [320, 360): it must not be heard, which a
  position counting the whole chunk would get wrong. Whole answers (with and without word
  timestamps) stay whole. A mid-stream barge-in (the LLM pauses 9 s before its third delta,
  16 seeded cases) leaves only the heard part of the text sent so far, and the tail never reaches
  the context.
- Estimate in the pipeline: runs without word timestamps and barge-ins at four points must equal
  an independently computed estimate (`floor(played x 14)` chars, backed up to a whitespace) and
  are flagged as estimated in the stats; a timed run never is. The VAD works in 20 ms steps, so
  the barge-in lands on a multiple of 20 ms.
- Chaos (`crates/brisvox-sim/tests/chaos.rs`): for TTS disconnects, the context must equal the heard
  prefix of the first answer exactly, matched token by token against the word log (the answer's
  words are distinct), and at least one disconnect must cut an answer mid-way with text heard.

**Production path.** The engine wires this through the output pacer's playout-position report
(at the barge-in instant) and the `PlayoutBook` (see PIPELINE.md and the PlayoutBook section
above); the simulation exercises the same contract.

## Transport
`crates/brisvox-transport` is the provider-neutral boundary between a caller and a session. It
has no socket code: a protocol adapter (websocket, Twilio, WebRTC) bridges the
provider protocol to two channels, and the core does the rest.

**Pieces**
- `Transport::accept` yields a `CallLink` per call. The adapter keeps the matching `CallEnds`
  (`call_pair`): `WireEvent`s in (caller audio, DTMF, mark echoes, fault, hang-up), `WireOut`s out
  (audio, clear, mark, hang-up), and `ended`, a token the core cancels when the call is over
  (an adapter blocked on its client must stop waiting and return).
- `TransportSession::open(clock, link, config, parent)` starts the session. The engine reads
  `TransportEvent`s (`recv`) and sends `TransportCommand`s (`send`, or a cloned `commands()`
  `CommandSender`). `close(timeout)` cancels and joins every task; dropping the session cancels
  it. `stop_reason()` returns why the session ended, even if the `Stopped` event was lost.
- `LoopbackTransport` + `LoopbackClient`: the in-memory transport. The client sends audio, DTMF
  and mark echoes and reads what the client would hear, with the time it heard it.
- `CallInfo::sample_rate()` parses `metadata[CallInfo::SAMPLE_RATE_KEY]` into
  `Result<Option<CallRates>>` (`CallRates { input, output }`, `CallRates::symmetric`): `Ok(None)`
  when the adapter reports no rate (the loopback), `Err` for a malformed or unsupported value. The
  engine falls back to `EngineRates::fallback_call` on `None`. The websocket adapter writes the key
  through the same constant, and a test pins it.

**Tasks of one session** (all children of the `parent` token)
- *commands*: accepts queued engine commands in order. `Audio` goes into the production
  `OutputPacer` (pipeline `transport.out`); `Mark` is recorded at the enqueued position; `Stop`
  records `Requested` and cancels. `Clear` is not queued (see Clear).
- *output*: the only writer to the wire. It takes frames from the pacer's sink, drops stale ones,
  writes `WireOut::Audio`, updates the released total and sends marks once the released audio
  reaches their position (never while a clear is waiting to be applied: a mark registered after the
  clear must not overtake `WireOut::Clear`). When the ledger's generation changed it sends
  `WireOut::Clear`, then reports the cut utterances' final positions and the cut marks. A position
  counts as reported only once its report is queued, so a cancellation in the middle leaves it for
  the final report.
- *input*: forwards `WireEvent`s to the engine, re-tagging caller audio with the current epoch.
- *supervisor*: joins the three tasks and the pacer pipeline, hangs up the client
  (`WireOut::Hangup`), cancels `CallEnds::ended`, and emits `Stopped{reason}`. The first task to
  end cancels the rest, with one exception: when the output task finds the wire closed (the adapter
  dropped its `outgoing` end), the input task keeps reading for up to 1 s, so a hang-up, key presses
  and mark echoes the adapter queued before it left are delivered and the stop reason is
  `PeerHangup` rather than `PeerDisconnected`. Past that second, or when the adapter's event side
  closes without a hang-up, the session ends as `PeerDisconnected`.

**Output timing**: `Audio` is released by the production pacer, so the wire never sees faster
than real-time audio. The pacer keeps its absolute 20 ms grid and underrun rules. Added
latency, from the engine's `Audio` command to the wire: the pacer's own queue (`pacer_capacity`,
headroom for synthesis, not delay), the pacer's sink (`sink_capacity`, default 4 frames = 80 ms
of queue when full, but the sink drains as fast as the wire accepts), the wire queue
(`WIRE_OUT_CAPACITY` = 8 frames when full), then the adapter's own buffer (for the loopback, its
`latency`, default 30 ms). Only the first is on the audio's schedule. Keep the sink and the wire
queue short: each queued frame is latency.

**Clear** is out of band. The audio path is deep (command queue 64, pacer input 64, sink 4, wire
queue 8), and a clear that waited its turn in it would wait for up to 2 s of audio. So
`CommandSender::send(Clear { epoch })` applies it before it returns, from the caller's task:
1. under the ledger lock it raises the epoch (accepted audio becomes stale) and moves every mark
   that was not echoed yet to the cut list (see Marks), and, still under the lock, pulls the stale
   `WireOut::Audio` and every queued `WireOut::Mark` out of the wire queue (`Sender::purge`; the
   output task sends only after it saw the new generation, so nothing newer is in the queue);
2. it wakes the output task;
3. it puts `Interrupt{new_epoch}` on the pacer's system lane, which purges the pacer's data lane
   and abandons the frame the pacer waits on; frames still in the sink are skipped by the output
   task.
The output task then writes `WireOut::Clear`, so the adapter drops what it buffered and tells the
client. Audio that was still in the command queue is rejected by its epoch when the command task
reaches it. Boundary: after the clear command, at most the one frame the output task was already
writing to the wire (its write was blocked, or in flight) still reaches the adapter ahead of
`WireOut::Clear`; the adapter drops it on the clear. Tested at the wire level (an adapter that
reads nothing, a full wire queue, 40 queued frames: at most one old frame after the clear) and
through the loopback (100 frames queued, the client is cleared in under one frame time). A
`Clear` with an epoch not newer than the current one is ignored. Audio must follow the `Clear` of
its epoch (epoch 0 needs none); audio of any other epoch is dropped. `Stop` is not out of band:
it takes effect when the commands queued before it were accepted, and then drops all pending
audio.

**Marks**: a mark is positioned at the audio accepted before it, and is sent to the client when
that audio has been released; the core then remembers it as sent (at most 256, the oldest is given
up and reported as `MarkCleared` past that). The client echoes it when it has played
(`MarkPlayed`). A clear cuts every mark that has not been echoed: waiting for its audio, queued on
the wire (purged with the stale audio) or buffered in the adapter (dropped on `WireOut::Clear`, or
echoed by a protocol like Twilio's; the core does not care). The engine gets `MarkCleared{id}` for
each, and an echo that arrives later is ignored (an echo of an id the core does not wait for is
dropped), so every mark ends in exactly one of `MarkPlayed` / `MarkCleared` (or in `Stopped`). A
mark still in the command queue when a clear is applied counts as cut: each queued command
remembers the epoch the engine sent it in. A mark whose echo was in flight when the clear arrived
is reported as cut, never as played: `MarkPlayed` stays "the caller heard everything before it".
Use distinct ids; an echo is matched to the outstanding mark with that id. The played-text logic can tell a cut
mark from a slow one.

**Playback positions**: `PlaybackPosition{epoch, utterance, played}` is the audio released to the
wire for the utterance, counted per `(epoch, utterance)` (the same utterance id in another epoch
starts at zero, and utterances may interleave without restarting each other). It is emitted every
`position_interval` (default 100 ms of audio), when another utterance takes over, on a clear (for
every cut utterance) and on stop. The last report of an utterance equals the audio released for
it; reports are monotonic per `(epoch, utterance)`. At most 256 counters are kept; the oldest is
reported and forgotten past that (an id that returns after 256 others restarts at zero).
Accuracy limits: (1) it counts audio handed to the transport, not audio the caller heard; it runs
ahead by the adapter's buffer (the loopback's latency) and a barge-in does not subtract what was
buffered; (2) a report is at most `position_interval` behind while audio is still playing and is
not emitted on idle; (3) audio that was purged from the wire queue by a clear after it was
released is still counted in the cut utterance's final position (up to `WIRE_OUT_CAPACITY`
frames). Exact playout is `MarkPlayed`; the telephony adapter reports marks, which is
what the played-text logic relies on for "what the caller heard". Between utterance-end marks an adapter
whose client reports its playout (Twilio) also sends `WireEvent::Lead`, which the session emits as
`TransportEvent::PlaybackLead{lead}` (`Displace`, latest wins): the audio sent to the client and
not yet played. `heard ~ PlaybackPosition.played - lead`. Loopback and the generic websocket send
none.

**Overflow policies** (`channel::Overflow`, one per item). Every queue is bounded:
- engine events (`events_capacity`, default 64): `Audio` (input) is `DropOldest` within its
  class, so a slow engine loses the oldest caller audio. `PlaybackPosition` is `Displace`: a new
  report removes the queued report of the same `(epoch, utterance)` (reports are cumulative, so
  nothing is lost but intermediate values), so the output never waits for the engine to read.
  `Stopped` is `Displace` too: it evicts the oldest input audio to get in. `Started`, `Dtmf`,
  `MarkPlayed`, `MarkCleared`, `Error` are `Backpressure` (never lost; the input task or the
  output task waits). The engine must therefore keep reading events while it uses the session,
  as long as it registers marks or expects DTMF; position reports alone cannot stall it.
- engine commands (`commands_capacity`, default 64): `Backpressure`. `Clear` does not use it.
- wire events (`WIRE_EVENTS_CAPACITY` = 64): `Audio` `DropOldest`, the rest `Backpressure`.
- wire out (`WIRE_OUT_CAPACITY` = 8): `Backpressure`; a slow client slows the output, the pacer
  falls behind, and it resyncs with an underrun, as it does for any slow sink.
- pacer input (`pacer_capacity`) and sink (`sink_capacity`): `Backpressure` (the framework's
  audio policy).
A slow engine therefore loses input audio (oldest first) and intermediate position reports, and
nothing else, except one case: if the event queue is full of lossless events and the engine reads
nothing for one second after the session ended, `Stopped` is dropped. The reason stays available
from `TransportSession::stop_reason()`.

**Cancellation, timeouts and shutdown**: every session task waits on channels inside a `select!`
with the session token. Bounded waits: the output task's wire write (`WIRE_WRITE_TIMEOUT`, 10 s:
an adapter that takes nothing for that long has stalled and the session fails with a timeout),
the final position reports and the hang-up and `Stopped` sends (1 s each). The command task's
pacer push waits without a timeout (the pacer is in-process and drains at real time), but it ends
with the token. Stop, peer hang-up and peer disconnect each record a `StopReason` (first writer
wins) and cancel. The supervisor reports the reason as the last event and cancels
`CallEnds::ended`; `close(timeout)` returns `Timeout` and aborts the supervisor if the tasks do
not finish in time. Held `CommandSender` clones do not keep the pacer or the wire alive after
that. The loopback adapter cancels every send (to its client and to the core) on the owner token
or `ended`, and bounds each by `LoopbackConfig::send_timeout` (default 5 s; past it the call
ends and the core reports a lost peer). A dropped `LoopbackClient` is a lost peer
(`PeerDisconnected`); `LoopbackClient::hangup` is `PeerHangup`.

**Tests** (`crates/brisvox-transport/tests/transport.rs`, paused time, `SimClock`; the channel
policy tests use a test item type): input audio and DTMF arrive in order; output is paced on the
20 ms grid and takes its real duration; a clear drops the queued old audio (the client hears no
old audio after the clear, and the cut utterance's final position is reported); a clear behind
100 queued frames reaches the client within a frame time; at most one old frame is written to the
wire after a clear (adapter that reads nothing, so the loopback's filtering cannot hide it); a
mark arrives right behind its audio and its echo becomes `MarkPlayed`; a mark cut by a clear
becomes `MarkCleared` and never reaches the client (also behind enough new audio to pass its
position, and a late echo of it is ignored), also when it was still in the command queue, and
also when it was already sent (in the adapter's buffer or left in the wire queue behind purged
audio); a mark registered after a clear never overtakes the clear on the wire; the cut utterance's
final position is exact (120 ms for a clear at 130 ms with seed 0) and comes from the clear, not
the interval; a report that a cancellation interrupts is still delivered at the end; the adapter
that queued a hang-up and left delivers it (and what preceded it) as `PeerHangup`; caller audio is
re-tagged with the current epoch and keeps `seq` and `timestamp`, DTMF keeps its time;
`WireOut::Hangup` is queued before `ended` is cancelled, also when the wire queue is full (bounded
by 1 s); an adapter that reads nothing for 10 s fails the session with a `wire write` timeout;
held `CommandSender` clones are refused after the end and keep neither the wire nor the pacer
open, and concurrent clears from two clones apply in epoch order; `DropOldest` evicts only its own
class (a lossless item in front of lossy ones survives; with nothing of its class queued the new
item is dropped and counted);
positions are monotonic and equal the released audio, also when utterances interleave and when an
utterance id repeats in a new epoch; an engine that never reads events can still send 20 s of
audio (positions coalesce); stop drops pending audio and ends both directions; stop and cancel
with a client that never reads leave no task behind (checked through `Arc::strong_count`); a
client that stops reading for too long ends the call; stale-epoch audio and an old clear are
ignored; hang-up and vanished client map to `PeerHangup` and `PeerDisconnected`; a protocol fault
becomes `Error`; `stop_reason()` survives a lost `Stopped`; a slow engine loses only input audio
and keeps DTMF in order; the channel's four policies and `purge` hold under a slow consumer.

## WebSocket transport
`brisvox_transport::websocket` (feature `websocket`, on by default) is the generic protocol
adapter on top of the sans-IO transport core. The wire protocol (messages, limits, close codes,
error codes) is in `docs/WEBSOCKET_PROTOCOL.md`; this section is the design.

**Pieces**
- `WebSocketServer` implements `Transport`. `bind`/`serve` start one accept task (tracked by a
  `TaskTracker`); `accept` yields a `CallLink` per connection that completed the upgrade and sent a
  valid `start`. `shutdown(timeout)` cancels, says goodbye (`stop{shutdown}` + close 1001) to every
  call and waits for all tasks. Dropping the server cancels without waiting.
- One connection = one spawned task. It does the handshake, reads `start`, builds the
  `call_pair` (queues `WireEvent::Started` first, hands the `CallLink` to the accept queue), then
  splits the socket and drives a reader and a writer future with `join!` in the same task. A
  stuck write therefore never stops reads (and vice versa) and there is exactly one task per call
  to leak-check.
- `CallInfo.metadata`: the client's `start.metadata`, plus `client_call_id`, `transport`,
  `sample_rate`, `encoding`, `peer_addr` (server keys win). The engine reads `sample_rate` to
  synthesize at the call's rate.
- `websocket::client::run` is the reference client (also the `ws_client` binary); `wav` is the
  ~100-line mono PCM16 WAV reader/writer both use.

**Reader** (client to engine): binary frame -> `AudioFrame` (pcm16 zero-copy from the websocket's
`Bytes`; mu-law decoded into one fresh buffer) -> `WireEvent::Audio` (DropOldest, never waits).
Text frame -> `dtmf`/`mark`/`stop` events. Any frame refreshes the liveness stamp; a single timer
fires at `ping_timeout` after the last frame and re-arms itself if frames arrived meanwhile (no
per-frame timer work). A close frame is `WireEvent::Hangup` (`PeerHangup`), a vanished socket
drops the events sender (`PeerDisconnected`). "Vanished" includes what tungstenite reports as
`Protocol(ResetWithoutClosingHandshake)` (EOF without a close frame) and any I/O error: they are
not the peer's protocol violation, so no fatal error is queued and the engine sees no
`TransportEvent::Error` (only genuine violations, such as a bad frame or invalid UTF-8, are
reported and answered with 1002). After a fatal fault or a hang-up the reader stops
feeding the core (drops its sender) but keeps reading, up to `close_grace`, for the peer's close
frame, so the close handshake completes and the peer does not get a reset that would discard the
error message in flight. A closing connection is not read before the writer has said goodbye
(`Conn::writer_done`): the websocket layer refuses every write after it has read the peer's close
frame, so reading it early would lose the `stop`. When the peer's close frame is read first (a bare
`ws.close()`), the layer queues the close reply and only a later read or flush sends it; the
writer's last step (`Writer::finish`) closes the sink, which flushes it, so the client always gets
a close frame back and never a reset (RFC 6455 5.5.1). A client that sends `stop` then close gets
its `stop{ended}` first.

**Writer** (engine to client): `WireOut::Audio` -> binary frame(s) in the call's encoding
(pcm16 zero-copy; frames longer than `max_audio_bytes` are split into chunks of at most that
size); `Clear{epoch}` -> `clear` message and a floor epoch (older audio is dropped); `Mark` ->
`mark` (an id over 256 bytes is not sent: non-fatal `mark_id_too_long`, since the client's echo
would be refused); `Hangup`/`ended` -> `stop{ended}` + close 1000; server cancel ->
`stop{shutdown}` + close 1001. A write that is blocked on the socket gives up the moment the server
shuts down or the core ends the call (`CallEnds::ended`), and the goodbye then gets at most
`min(write_timeout, close_grace)`, so a stuck client holds the call's permit for about a second
after a hang-up, not for several write timeouts. Bot audio at another sample rate than the call's
is dropped with one non-fatal `output_rate_mismatch` per call: resampling is DSP and belongs to the
audio layer, not to a transport task. **Known limitation:** a call negotiated at 8 kHz or mu-law
stays mute unless the engine synthesizes at the call's rate (`CallInfo.metadata["sample_rate"]`).
Follow-up: resample bot audio to the call's rate on the inference pool before the transport
(see `docs/WEBSOCKET_PROTOCOL.md`, "Audio"). The writer also sends the server pings and the reader's error reports (a bounded
queue of 8: non-fatal notices are dropped when it is full, a fatal report waits up to
`write_timeout`).

**Flow control and overflow** (all bounded, see also the transport core above): connections `max_calls` (a 503 with
`Retry-After` past it, answered by at most 32 concurrent reject tasks; beyond that connections are
dropped), accept queue (`accept_queue`, wait `send_timeout`, then `server_busy`), frames
(`max_audio_bytes`, `max_control_bytes`, enforced by the websocket layer and again per message
kind), input audio DropOldest, control events lossless with `send_timeout` (then `overloaded`),
output through the 8-frame wire queue with `write_timeout` per socket write (then the call is
closed: playout is real time, so a client that falls behind is lost rather than buffered).
At most 16 non-fatal faults per call.

**Configuration**: `ServerConfig::validate` (called by `bind`/`serve`, `Error::Config`) refuses
`ping_timeout <= ping_interval`, `ping_interval < 10 ms`, zero timeouts or `close_grace`,
`max_calls`/`accept_queue` of 0, `max_audio_bytes < 2` and `max_control_bytes < 512`.

**Timeouts** (every network await has one): handshake, start, each write, each event delivery,
the close handshake, the accept hand-off, the shutdown wait. All of them, and the ping and idle
timers, run on the injected `Clock` (`websocket::timeout` races the future against
`clock.sleep`), so a simulated clock drives them all; the reference client does the same. Only the
socket I/O is real. Tests: the start, handshake and ping timers run on a `SimClock` (paused tokio
time with real sockets; a busy task keeps tokio from auto-advancing while it waits for I/O) and
fire only when the test advances the clock, never when real time passes; a source scan fails the
build of the tests if the websocket module uses `tokio::time::timeout`/`sleep`/`interval` or real
instants outside the clock. Hot path per frame: no allocation for pcm16 in or out; the mu-law path
converts into a per-connection `BytesMut` that is allocated in 16 KiB chunks and split per frame
(51 frames per chunk for the 320-byte decoded frames of 20 ms at 8 kHz, 102 for the 160-byte
encoded ones: about one allocation per 50 frames in and per 100 frames out, instead of one per
frame; a unit test in `connection.rs` keeps 300 frames and checks their content and the number of
allocations); the cancellation futures each
side polls in its main loop are created once per connection, not once per frame (a write
creates its own interrupt future, which does not allocate).

**Reference client**: the send loop never starves the receive side. After every sent frame the
select loop looks at the socket once (needed in `--no-realtime`, where the next frame is always
due), and a write that cannot complete (the server is blocked writing to the client, so its
receive window is closed) keeps serving the server's messages while it waits (`send_reading`);
otherwise a long input sent at full speed against a server already producing audio would
deadlock both directions until the client's write timeout. The server's messages are applied in
one place (`Call::on_server`), whether they arrive between two sends or during one.

**Tests** (`tests/websocket.rs`, real sockets on 127.0.0.1:0, the real clock unless noted): handshake and
`started`; invalid starts; audio both ways in order (pcm16 and mu-law); clear drops queued audio
and cuts marks; mark round trip; DTMF; malformed JSON, oversized and half-sample frames, a second
`start`, unknown message types (non-fatal, counted); a flooding client (input drop-oldest, call
survives); the call limit (503 and slots freed); ping timeout and pings keeping an idle client
alive; a client that reads nothing (12 MB frame) is dropped after `write_timeout` while the
engine still hears it; hang-up by stop, close frame, abrupt drop and engine stop; shutdown;
`TaskCounter` leak check across all of it; the reference client and the `ws_client` binary against
an echo engine (byte-exact recording, mu-law, clear handling, refused start). Added by the review:
a client close frame and a stop-then-close are answered with a close frame (no reset); a stuck
client does not hold the permit past an engine hang-up or a server shutdown; the accept queue full
(`server_busy`), an engine that reads no events (`overloaded`), the reject slots running out,
oversized `start`, long mark ids, a rate mismatch lasting seconds, configuration validation, the
app-level audio limit; the leak check across fatal closes, a stuck client and rejected
connections; the reference client against scripted servers (connection vanishing mid-stream, a
fatal error mid-stream, the maximum duration, a bot hang-up, mark echo timing and clear), and the
`ws_client` exit code 1 with the recording still written. Added by the second review: every
close code of the protocol table (1000, 1001, 1002, 1008, 1009, 1013) is asserted at its call site;
an abrupt disconnect (before and after `start`) gives `PeerDisconnected` and no engine error; any
client frame keeps the call alive (audio only, no pongs); the start, handshake and ping timers on
a `SimClock`; the clock-injection source scan; the reference client reading while it sends
(`--no-realtime`, 16 MiB of input against 32 MiB of bot audio) and serving a mark between
frames; `failure()` for a clean close before and after the end of the input; the `ws_client`
options (`--encoding mulaw`, `--dtmf`, `--meta`, `--no-realtime`, `--linger-ms` with a measured
run time, the WAV's sample rate) and its exit code 2 cases (unknown option, bad values, missing or
invalid input, unwritable output).

## Twilio transport
`brisvox_transport::twilio` (feature `twilio`, on by default) speaks Twilio Media Streams on the
infrastructure of the generic WebSocket server; protocol facts, TwiML and the UNVERIFIED list are
in `docs/providers/twilio.md`.

**Reuse**: `TwilioServer` wraps a `WebSocketServer` built with `serve_with(.., ServeFn)`. The accept
loop, call limit (503), reject slots, accept queue, shutdown and `Transport::accept` are the same
code; only the per-connection function differs. `ServerConfig` (limits, ping, idle, write, send
and close timeouts) applies unchanged; `TwilioConfig` adds the path, the auth token and public URL,
the outbound chunk size and the progress-mark interval. All timeouts and timers run on the injected
`Clock`.

**Connection** (one task per call, as in the generic websocket transport): upgrade callback (path -> 404, signature -> 403),
read `connected`/`start` within `start_timeout`, validate the media format, build the `CallInfo`
(`call_id` = callSid, metadata in the provider doc), `call_pair`, hand the link to the accept
queue, then split the socket and run a reader and a writer with `join!`. Shutdown is symmetric as
in the generic websocket transport (`reader_done`, `writer_done`, `close_grace`); the farewell is only a close frame, because
Twilio takes no `stop`/`error` messages from the server. The reader asks the writer for a fatal
close over a 2-slot queue (first wins); the writer reports a fault (rate mismatch) to the reader
over an 8-slot queue (full: dropped, the reader owns the engine's event queue).

**Audio**: inbound base64 is decoded into a reusable `Vec`, mu-law into PCM16 in a 16 KiB-chunk
`BytesMut` (`scratch_bytes`, shared with the generic websocket transport); outbound PCM16 is encoded the same way and
each 160-byte piece becomes one `media` message built in a per-connection `BytesMut` (fixed
prefix with the pre-escaped `streamSid`, base64 straight into the buffer, `Utf8Bytes::try_from`
on the frozen slice): no per-frame allocation beyond one per ~60 messages. Parsing borrows from the
frame (`Cow`), numbers that Twilio prints as strings go through a visitor, no allocation per
`media` message.

**Marks** (`twilio::marks::MarkBook`, shared by reader and writer behind a `std::sync::Mutex`
that is never held across an await): engine marks `e<n>` (id kept locally), progress marks
`p<n>`, at most 512 in flight (oldest forgotten, counted), `clear()` forgets all of them. Echoes
resolve against the book: engine mark -> `MarkAck`, progress -> latency statistics, unknown ->
dropped. The core's own ledger reports `MarkCleared`; the adapter only guarantees an echo of a
flushed mark is never taken for a played one. `TwilioServer::stats()` exposes shared atomic
counters (media in/out, lost inbound chunks, marks sent/echoed/cut/unmatched/evicted, clears,
faults, max echo latency, max unplayed audio).

**Bounds**: calls and accept queue as in the generic websocket transport; frames `max_control_bytes` (text) and
`max_audio_bytes` (decoded payload); faults 16 per call; marks 512 per call; every channel is
bounded with the policy named in the code. Slow Twilio (a write over `write_timeout`) closes the
call.

## Telnyx transport
`brisvox_transport::telnyx` (feature `telnyx`, on by default) speaks Telnyx media streaming
(bidirectional `rtp` mode) on the same server infrastructure as the Twilio adapter; protocol facts
and the UNVERIFIED list are in `docs/providers/telnyx.md`.

**Shared code.** What both telephony adapters use moved to `telephony` (crate private, compiled for
`twilio` or `telnyx`): `MarkBook` / `Echo` / `MarkName` (re-exported by `twilio::marks` and
`twilio::protocol`), the lenient number readers (`Lenient`, `Soft`, `SoftText`), the base64 engine,
and the upgrade/refusal helpers (`refusal`, `blame`, `refuse`, `close_reason`). The reader/writer
loops are a copy of Twilio's with the differences below (their wire formats, auth and ending differ
too much for one generic loop to be cheaper than two).

**Differences from Twilio.**
- *Wire*: snake_case, `stream_id`; the server's `media`/`mark`/`clear` carry no stream id (Telnyx's
  schemas forbid extra properties). `sequence_number`, `chunk`, `timestamp` are strings (RTP
  timestamp, not ms). `error` frames (100002..100005) become engine faults.
- *Codecs*: PCMU or PCMA at 8 kHz (inbound from `start.media_format`, outbound from
  `TelnyxConfig::codec`, which must equal `stream_bidirectional_codec`); L16/G722/OPUS/AMR-WB are
  refused at `start` (L16 byte order is undocumented, the rest need codecs not in the tree).
- *Chunks*: Telnyx takes 20 ms to 30 s per message, so `chunk_bytes` is at least 160 and a tail under
  20 ms is carried (<= 159 bytes, no allocation per frame) to the next audio, silence-padded when
  an engine mark follows it, dropped by `clear`.
- *Order*: frames are not ordered. `telnyx::reorder::Reorder` (sans-IO, `BTreeMap`) holds at most
  `reorder_window` (default 8, max 64) frames; an in-order frame never touches it (no copy). A gap
  is given up on when the window overflows or `reorder_timeout` after the first frame behind it
  arrived (injected clock; one timer in the reader's select, re-armed after every message and flush
  from the arrival times of what is still held, so a later gap keeps its own full wait). Late and
  duplicate chunks are dropped and counted; the first frame anchors the numbering. A chunk with an
  unusable payload (empty, bad base64, too large) is offered with no data so it fills its place.
  Chunks more than 1500 ahead or behind the one due are dropped; more than `max(window, 4)` of them
  in a row re-anchor the buffer on the newest (`chunk_resyncs`, one engine fault), so a corrupt or
  restarted counter cannot silence the caller. Audio still held is released before a `stop` or
  close is delivered. `dtmf` waits up to `reorder_timeout` in a bounded list (32) and is delivered
  in `sequence_number` order (`dtmf_reordered`). Frames that overtake `start` are dropped (counted,
  at most 64).
- *Auth*: no signature. With `auth_token` (Telnyx's `stream_auth_token`): a wrong
  `x-telnyx-streaming-auth-token` upgrade header is 401; a missing header defers to
  `connected.x-telnyx-streaming-auth-token`; without proof by `start` the connection is closed 1008
  and the engine never sees it. Comparison: HMAC-SHA256 of both under a fixed key, compared with
  `ring::hmac::verify` (constant time, length independent). The token is `Secret` in the config and
  redacted in the parsed `connected` message.
- *Hang-up*: closing the socket is not documented as a hang-up. `TelnyxConfig::hangup` is a
  host-supplied async hook (REST `hangup`; brisvox holds no Telnyx API key), called once, bounded by
  `hangup_timeout` on the injected clock, before the close frame, whenever the adapter ends the
  connection (engine hang-up, fatal fault, idle timeout, shutdown, broken socket, a bare close
  frame from Telnyx, which is not documented as ending the call) and for a call refused after
  `start` (unsupported media format, engine not taking calls), but not after Telnyx's `stop` and not
  for authentication or duplicate refusals. Telnyx `error` frames are reported as faults but do not
  count toward the 16-fault limit.
- *One stream per call*: `telnyx::registry` remembers live and the last 1024 ended
  `call_control_id`s; a second connection (TeXML reconnect) is closed 1008. A claim is remembered
  only once the call was handed to the engine, so the retry after a 1013 is accepted. The upgrade
  path is compared in constant time (it may be the only secret).
- *Stats*: `TelnyxStats` adds `media_reordered`, `media_late`, `media_before_start`,
  `telnyx_errors`, `duplicate_calls_refused`, `hangups_requested`, `hangups_failed`,
  `hangups_skipped`, `chunk_resyncs`, `dtmf_reordered`.

## WebRTC transport

*A second door on the same port:* a WebSocket upgrade on `BRISVOX_WEBRTC_WS_PATH`
(`webrtc/ws.rs`). The request is read by the signaling HTTP parser, checked (upgrade headers, `Origin`,
the listener's credential, offer rate, call slot, handshake slot, per-address call cap), answered `101`
there, and the raw stream is handed to the websocket transport (`websocket::connection::run_upgraded`,
the code the generic listener runs), with the WebRTC server's `calls` queue as its sink. The call has
no media path, no UDP and no codec: it is a generic websocket call that shares the WebRTC listener's
admission. Details: `docs/DEPLOY.md` "WebSocket fallback", `docs/SECURITY.md`.
`brisvox_transport::webrtc` (cargo feature `webrtc`, off by default: it needs a C toolchain and cmake for
libopus) lets browsers talk to the engine. Facts, measurements and the UNVERIFIED list are in
`docs/providers/webrtc.md` (section 10); the library choice (str0m + libopus) is recorded there.

**Parts.** `WebRtcServer` (a `Transport`) owns three kinds of tasks: a signaling accept loop (minimal
HTTP/1.1 on a `TcpListener`: WHIP-style `POST`/`DELETE`/`OPTIONS`, a demo page), one UDP reader for the
single shared media socket, and one task per call. `demux` routes a STUN request by the ufrag it carries (`<server ufrag>:<browser ufrag>`; the ufrag wins over a
known source, so a client reusing its 5-tuple for a new call reaches the new call) and every other datagram by
its source address (least-recently-used list of 8 per call, refreshed by every datagram); each call has a
bounded inbound queue (`WebRtcConfig::inbound_queue`, drop-oldest, exactly sized copies so the bound is in bytes
too, overflow visible live in `stats()`). `peer` drives one sans-IO `str0m::Rtc` per call: drain
`poll_output` (transmit with `try_send_to`, never waiting), then select over cancel, DELETE, the engine's
`ended` token, the inbound queue, the engine's `WireOut` queue and one timer (the `Rtc`'s timeout, the
connect deadline, the disconnect grace, the next mark acknowledgement). Time is the injected `Clock`
(`tokio::time::Instant` converted with `into_std`).

**Call life.** `POST` -> permit (call limit, 503) -> per-address count (429) -> `spawn_blocking`: parse the
offer (mDNS candidates removed), `Rtc` with ICE-lite, the server's host candidate, the shared DTLS certificate,
`accept_offer`, `MediaPath` -> `201` with the answer and `Location`. The engine hears about the call only when
`Event::Connected` fires (ICE and DTLS up): the `CallLink` goes into the accept queue with `try_send` (a full
queue ends the call), then `WireEvent::Started` with `sample_rate` (input) and `output_sample_rate`
(`CallInfo::sample_rate` now reads both; absent output = input). An offer whose browser never connects is
abandoned after `connect_timeout` and was never visible to the engine. A `SessionGuard` removes the registry
entry, the per-address count and the permit on every exit, including a panic (`catch_unwind` around the call:
`call_panics`).

**Audio.** `MediaPath` (sync, no sockets) holds the Opus decoder and encoder and two `StreamResampler`s: 48 kHz
-> input rate on the way in (loss concealed from the timestamp gap when `contiguous` is false, at most
`max_concealed_frames`), output rate -> 48 kHz -> 20 ms frames -> Opus on the way out. Codec work (encode 90-376 us,
decode 25-45 us a frame) runs on the inference pool when `WebRtcOptions::codec_pool` is set, else on the call task
(`serve` warns when there is no pool and `max_calls` is above 8); the path sits behind `Arc<Mutex<..>>` so a
refused, timed-out or panicked pool job drops its frame but loses no codec state (a poisoned lock is recovered; the
call goes on). Inbound audio is rate limited per call (`max_audio_packets_per_sec`, token bucket, default 200,
excess dropped before the codec and counted), which bounds the codec work one client can cause. Bot audio at another rate is
dropped with one fault.

**Interruption.** `Clear` resets the resampler, the partial frame and the encoder and forgets pending marks;
what is already on the wire plays out. `Mark` flushes the held audio, then is acknowledged `mark_delay` later
(browsers report no playout). RTP timestamps follow the media, with a jump and marker bit after a pause (tested on
the received RTP time and talkspurt bit).

**Bounds and faults.** Calls (`max_calls`), per-address calls, signaling connections, body 64 KiB, header 8 KiB,
one request per connection, deadlines on every read and write, bounded queues with named policies, accept
queue. A stuck engine stalls its own call's task for at most `send_timeout`, then the call ends.

**Hosted by the binary** (`brisvox-server` feature `webrtc`, `docs/OPERATIONS.md` "WebRTC"):
`BRISVOX_WEBRTC_*` configure a `WebRtcServer` that `hosting::Listeners` runs like the other
transports, as a child of the manager's shutdown token. The transport has no admission gate, so the
accept loop admits a call when it connects and hangs up when the manager is full (the other
listeners refuse before the upgrade). The Opus codec runs on the inference pool
(`HostSettings::with_codec_pool`, set by `main` from the pool it creates). `examples/web-assistant`
is a static page for it; it was connected to the hosted listener from Chrome once, by hand.

**Tests** (`tests/webrtc.rs`, real sockets on 127.0.0.1, a second `str0m` peer plays the browser): signaling
round trip and every refusal, auth, CORS, demo page, call limits, ICE-lite and full-ICE connectivity, audio both
ways through `TransportSession` with a mark, a clear and a new epoch, the inference-pool path, DELETE and DTLS
close as hang-up, a sendonly offer, rate mismatch, abandoned offers, full accept queue, several calls, shutdown
without leaked tasks, loss concealment on a lossy wire and no concealment for DTX jumps, a packet flood against the
rate limit (timers still run), the RTP time line, the latency median, pool panic/timeout/refusal, and a vanished
browser (simulated time, no `#[ignore]`).

**Reachability.** The server is ICE-lite, so a call needs a path to the server. Two additions, both
opt-in, both in new modules so `signaling.rs` stays small:
- *ICE-TCP, passive* (`webrtc/tcp.rs`, `WebRtcConfig::tcp_addr`): a second host candidate
  (`typ host tcptype passive`, lower priority than UDP: str0m host/UDP 126, host/TCP 90). A browser that cannot use
  UDP connects to it and speaks the same STUN/DTLS/SRTP in RFC 4571 frames (2-byte big-endian length). The listener
  is unrouted per connection: the first STUN request names the call by its ufrag (`Registry::by_ufrag`, the lookup
  the UDP socket uses) and pins the connection to that call; from then on frames go to the call's inbound queue as
  `Datagram { proto: Tcp, reply: Some(writer) }` and the call's `str0m` sees `Receive::new(Protocol::Tcp, remote,
  tcp_candidate, ..)`. The call remembers (remote address, writer) per connection (at most 4) and frames
  `Transmit`s with `proto == Tcp` onto that writer's queue. Bounds: `max_tcp_connections` (extra connections are
  closed unanswered), `tcp_idle_timeout` (absolute deadline for a connection that has not named a live call; junk
  does not extend it), 30 s without a complete frame once pinned, `tcp_max_frame` (a longer announced frame closes
  the connection before the body is read), a fixed 4 KiB read buffer per connection, an outbound queue of
  `tcp_queue` frames with *drop-oldest* (whole frames only, so the stream stays framed; the call task never waits on
  a socket), the write timeout of the config, and the connection closes when its call leaves the registry.
  One allocation per outbound packet (header plus payload); the path is a fallback, not the hot one.
- *Browser TURN credentials* (`webrtc/turn_creds.rs`, `WebRtcConfig::turn`): the server never uses TURN for the browser's
  media (its own TURN client is described below); it gives the *browser* short-lived relay credentials, as `Link: rel="ice-server"` headers on
  the WHIP `201` (RFC 9725 section 4.6) and as JSON from `GET <whip_path>/ice` (same bearer check as `POST`,
  `no-store`). coturn: computed locally (`<expiry>:<session id>`, HMAC-SHA1). Cloudflare / Twilio: one REST call
  under one lock (so concurrent requests share it), 5 s deadline on the injected clock, 60 s cache, a failure
  remembered for 10 s; provider answers are validated before they reach a header. A provider failure degrades to
  "no relay", never to a failed call. A *browser* TURN relay still needs the server's UDP port; hosts without UDP
  need the server's own TURN client (below).
- *Server-side TURN client* (`webrtc/relay.rs`, `WebRtcConfig::turn_relay`, `BRISVOX_WEBRTC_TURN_RELAY`):
  for hosts with HTTP(S) only (a one-port PaaS such as Railway). Per call, before the answer is built, the server opens a TCP or TLS
  connection to the TURN service of `WebRtcConfig::turn` (`turns:...:443?transport=tcp` preferred), allocates a UDP
  relayed address, and creates a permission for every IP in the offer's candidates (the browser sends the first
  check, and a TURN server drops data from peers without a permission). The relayed address is added to the call's
  `Rtc` as a **host** candidate (ICE-lite refuses relayed local candidates), after the direct one so `str0m` gives
  it a lower local preference; in `only` mode there is no direct UDP candidate. One task per call owns the
  connection and the sans-IO client (`turn-client-proto`): it splits the TURN byte stream into frames itself (STUN
  or ChannelData with its stream padding; garbage, a bad length or a frame above `max_frame` end that allocation
  only), turns Data indications and ChannelData into `Datagram { local: Some(relayed) }` on the call's inbound queue
  (drop-oldest, like UDP), and wraps the call's outgoing `Transmit`s (`source == relayed`) as ChannelData (channel
  bound on the first packet, at most 8 peers) from a bounded drop-oldest queue; the call task never waits on this
  socket. Refreshes (allocation, permissions, channels) run on the injected clock, `Refresh` with lifetime 0 when
  the call ends. Setup is bounded by `setup_timeout` (3 s): on failure the call answers with its direct candidates,
  or in `only` mode `503`. Counters `relay_*`. Tests: `tests/webrtc_relay.rs` against `turn-server-proto` in-process
  over TCP and TLS. Facts, sources and the crate's deviations (unpadded ChannelData, USERHASH): `docs/providers/webrtc.md`
  section 12.
Tests: `tests/webrtc_tcp.rs` (a str0m client whose only candidate is TCP-active completes ICE and DTLS and carries
audio both ways with `datagrams_in == 0`; frames cut into 5-byte pieces and several frames per write; oversize
frame; unknown ufrag closed after the idle timeout despite junk; the connection limit; close on call end; both
candidates in the answer with UDP preferred), `tests/webrtc_turn.rs` (coturn formula and `Link` headers, `GET
/whip/ice`, auth/CORS, Cloudflare and Twilio against a local mock of their REST APIs: request shape, caching,
failure and hang degrade, one shared fetch).

## RTVI transport
`brisvox_transport::rtvi` (feature `rtvi`, on by default) serves the Pipecat client SDKs
(`@pipecat-ai/client-js` + websocket transport) on the generic websocket server infrastructure, like the Twilio adapter:
`RtviServer` wraps a `WebSocketServer::serve_with`. Facts: `docs/providers/rtvi.md`; usage and the
manual check with a real JS client: `docs/RTVI.md`.

**Wire**: every message is ONE binary websocket frame holding ONE protobuf `pipecat.Frame`
(`rtvi::proto`, prost messages derived by hand, no protoc). RTVI JSON is a string in
`Frame.message`, audio is `Frame.audio` (PCM16 mono). The server never sends text frames (the JS
client throws on them) nor field 5 (`InterruptionFrame`, unknown to the JS schema).

**Rates**: asymmetric. The call reports 16 kHz in / 24 kHz out through the additive metadata keys
`input_sample_rate` / `output_sample_rate` (`CallInfo::sample_rate` reads them and falls back to
`sample_rate`, so the engine pump needed no change). The JS player ignores the frame's
`sample_rate`; as everywhere the adapter never resamples and drops bot audio at another rate.

**Handshake**: the call is handed to the engine after `client-ready` (bounded by `start_timeout`),
then `bot-ready` echoes the request id. Version check as Pipecat (warning `error-response`, call
continues).

**Side channels**: the core's event enums have no place for RTVI application messages, so a call
is an `RtviCall { link, sender, inbox }`. `RtviSender` (bounded 64, waits up to `send_timeout`)
carries typed `RtviEvent`s and replies to the writer; `RtviInbox` (bounded 32, `try_send`: full or
unread means the adapter answers the request with `error-response` itself) carries
`client-message`, `send-text` and `llm-function-call-result`. `dtmf` and `disconnect-bot` map to
core events.

**No playback ack in RTVI**: `rtvi::playout` estimates the client's playout on the injected clock
(`ends = max(ends, now) + duration`); engine marks are acknowledged when the estimate reaches them,
the estimate is reported as `WireEvent::Lead`, and `bot-started-speaking` / `bot-stopped-speaking` /
`bot-interrupted` are derived from it (`RtviConfig::auto_events`, switchable per call with
`RtviSender::set_auto_events`; the engine's `RtviBridge` turns it off when it sends them). A
barge-in drops the adapter's
queued audio but cannot flush what the browser already holds.
