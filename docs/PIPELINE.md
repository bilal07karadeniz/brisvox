# Production voice pipeline

Designed 2026-10-08 from three independent proposals (latency-first, correctness-first,
test-first) that were judged and merged. This file is the design reference for the production
pipeline; section 12 records how each part was built.

## 0. Decisions that settle the proposals

Base: the Proposal 2 process (Exact and Budget profiles, hybrid-chain rollout, `Segmenter::push_spans`) and the Proposal 1 correctness machinery. Latency choices come from Proposal 0.

| # | Decision | Source and reason |
|---|---|---|
| D1 | New crate `brisvox-engine` depends on core, audio, inference, agent, providers and transport. It dev-depends on `brisvox-sim`. Sim never depends on engine. The reference pipeline in sim stays untouched as the golden Exact baseline. | All three proposals |
| D2 | Self-wake core change is minimal: `SystemFrame::Wake{token:u32}` plus `ProcessorContext::self_sender()`. The payload travels in a typed, bounded `EventSlot<T>` (`Arc<Mutex<VecDeque>>`) owned by the stage. Frame stays 64 bytes and keeps its derived `PartialEq/Eq`. Wake is system lane, never purged, and never forwarded. | Keeps TurnDriver single-threaded and deterministic in one ordering domain. Avoids the manual `Eq`/`Debug` impls that `Custom(Arc<dyn Any>)` would need. Fallback: `Custom`. |
| D2a | `EventSlot::push_and_wake` coalesces wakes per slot with three states (idle, sending, queued). A push skips its send only while a `Wake` is already queued in the lane; a rejected push on a non-empty slot still wakes unless one is queued. A helper that finds another helper still sending sends its own wake instead of trusting it (that send may be cancelled), so wakes in flight are bounded by the number of helpers awaiting the lane, never by the number of pushes, and no queued event is left without a wake behind it. The stage must empty the slot on every `Wake`. | Review finding: the first design (one `wake_pending` flag, rejected pushes never wake) could strand events permanently after a cancelled helper. The conservative option costs at most one extra harmless wake per concurrent helper. |
| D3 | `RunningPipeline::stats()` exposes per-stage `QueueStats` and `stale_dropped`. | P1 |
| D4 | STT sits before TurnDriver, and everything flows downstream. TurnDriver feeds the Smart Turn ring from audio that STT forwards (a refcounted `Bytes` clone). Upstream traffic is only rare control frames: `TtsStarted` and `TtsEnded`. | Removes the upstream-finals deadlock surface of P1. Resolves the "STT consumes audio" contradiction in P2. |
| D5 | Two things were rejected: seq packing, and the find-from-cursor whitespace hack. Sent text comes from `Segmenter::push_spans`, which is additive in agent. | Judges |
| D6 | Utterance identity reaches TransportOut through a new `ControlKind::UtteranceStarted{utterance}`. TtsStage emits it from its reader task immediately before the first audio frame of each utterance. A test verifies that the control frame shares the data-lane ordering and fits the 64-byte cap; if it did not, the fallback would be seq packing. | Typed, no side channel |
| D7 | Model work (VAD, Smart Turn, large assembler finish) uses `Offload::{Pool(PoolHandle), Inline(SharedClock)}`. The sim uses Inline with an `EnergyModel: SpeechModel` and a scripted `TurnModel`. Real Silero and Smart Turn are covered only by real-time tests and the bench. | P0 |
| D8 | No second pacer. `TransportSession::open` already contains `OutputPacer`. The pipeline tail is TransportOut. | All proposals |
| D9 | Cross-stage side channels are limited to three. Each has a D-entry. (a) `PlayoutBook` (std Mutex, written at word and 10 Hz position rate). (b) The `CommandSender` for the fast-path Clear, held by TurnDriver and TransportOut. (c) `Arc<Mutex<Context>>`, shared with the sim probes. | |

## 1. Stage list and frame flow

```
TransportSession.recv()
   |  pump task (TaskGuard; always drains recv; started AFTER pipeline.start(), it needs
   |  input() and upstream_input())
   |    Audio    -> retag epoch ZERO, try_send into first-stage queue
   |    Started  -> parse metadata["sample_rate"] into CallRates{input, output}
   |    Dtmf     -> DataFrame::Dtmf in epoch 0 (the TurnDriver re-tags it with the current epoch)
   |    MarkPlayed / MarkCleared / PlaybackPosition -> PlayoutBook
   |       (+ for the end-of-utterance mark: Upstream Control TtsEnded via upstream_input())
   |    Stopped (and Error only with hangup_on_error) -> Hangup path (SystemFrame::Stop; lossless frames wait in one FIFO per
   |       target lane, so a DTMF stuck on a full data lane never holds back Stop or TtsEnded)
   v
[1 AudioIn]   KeepAll, e0, REALTIME_INPUT queue (data cap 8 = 160 ms, drop-oldest, counted)
              resample call rate -> 16 kHz (StreamResampler, inline <50us, equal rate = copy)
              forward 16k AudioFrame immediately
              tee a COPY of the 16 kHz samples (about 640 B) into a recycled Vec<i16> for VadWorker
              (bounded, drop-oldest, gap flag; a Bytes::from(Vec) clone would allocate a Shared box)
   .. VadWorker (helper task, TaskGuard): accumulates all queued frames (cap 8) into ONE job in flight
   .. offload.run(timeout, move |..| { stream.push(..); (stream, events) })
   .. emits backdated UserStartedSpeaking/UserStoppedSpeaking via ctx.sender(Downstream) (system lane)
   .. opt-in (VadSetup::with_early_analysis): also UserSpeechFalling{at} once per fall, when the
      probability stayed below the negative threshold for fall_delay, before min_silence confirms the stop;
      `at` is exactly the time the stop of that fall will carry. SttStage forwards it (its `_` arm), the
      TurnDriver consumes it: it snapshots the Smart Turn ring and analyses early, and reuses the verdict
      at the stop of the same fall (see TurnDriver module docs, "Early analysis")
   v
[2 SttStage<P>]  KeepAll, e0. Connects at Start (prewarm).
              audio -> provider via try_send (drops counted), and forwarded downstream
              on UserStoppedSpeaking: forward the stop, then send Finalize at once (speculative)
              reader task: Transcript (interim lossy, final reliable) + ControlKind::UtteranceFinal
              exactly one UtteranceFinal per Finalize, also on timeout or dead session
   v
[3 TurnDriver]  KeepAll. SOLE EPOCH AUTHORITY. Owns TurnMachine, SmartTurn ring, timer slot, finals gate.
              consumes audio (feeds ring), VadStart/Stop, Transcript, UtteranceFinal, Wake events,
              upstream TtsStarted/TtsEnded, Stop
              emits Interrupt (system), Control UserTurnStarted,
              then (after the gate) Transcript{final, joined} + Control UserTurnEnded
   v
[4 LlmRunner<L>]  DropStale. Per-epoch round task. -> LlmResponseStarted, LlmText deltas, Flush,
              LlmResponseEnded
   v
[5 SegmenterStage] DropStale. -> LlmText chunks (raw slices)
   v
[6 TtsStage<T>]   DropStale. -> UtteranceStarted, Audio (20 ms, call output rate), Word, TtsEnded
   v
[7 TransportOut]  KeepAll (as built: it filters epochs itself so it can count what it
                  drops; see section 12). Frame -> TransportCommand. Tail. Not a pacer.
```

- **Queues.** Audio stages use data cap 4–8 and system cap 32. Lossy: the VAD tee, STT audio send, and the pump-to-stage-1 queue. Lossless: control events. Bot-side audio uses Backpressure, bounded by sink_capacity 4 and the wire queue.
- **Upstream rule (core test).** Upstream pushes come only from helper tasks, the pump, or a non-blocking `try_send` inside `process()`. They are never an awaited push from `process()`. A core stress test of a bidirectional chain with a slow consumer proves there is no deadlock.
- **Helper-task rule.** Helper tasks are TaskGuard-owned and abort when the processor drops. They send through `GuardedSender{FrameSender, EpochWatch, epoch}`, which checks `latest() <= epoch` immediately before each send.
- **System-lane overtaking.** VAD Stop reaches STT ahead of queued audio. This is safe because Stop is backdated at least min_silence behind the last speech sample, so unsent audio is trailing silence. A test with a deliberately slow STT inbox confirms that Finalize never precedes real speech audio.

## 2. Ownership of epochs and turns

- TurnMachine in TurnDriver bumps the epoch at every new user turn. `Interrupt{new_epoch}` is emitted only on a real barge-in. User-side stages (pump, AudioIn, VadWorker, Stt) stay on epoch ZERO with KeepAll. Bot-side stages are DropStale and adopt the epoch from frames.
- **Ordering on a barge-in:** `Interrupt` always precedes `UserTurnStarted`.
- **TurnDriver is one Processor.** Frames and Wake events are handled in the same task. Wake events carry timer ticks, Smart Turn verdicts and finals-gate deadlines. All times come from `clock.now()` and `clock::wait_until`. There is no `Instant::now()`.
- **Action mapping:**
  - `StartUserTurn` pushes `UserTurnStarted`.
  - `RequestAnalysis{id}` takes `SmartTurn::snapshot()` (an owned `Vec<i16>`) and spawns a job through Offload that calls `SmartTurn::analyze_snapshot_blocking` (the engine's `Offload` is the one place jobs go, and `brisvox-inference` cannot depend on it; see section 12). The verdict returns as a Wake event. A stale or failed verdict is ignored, and the fallback_silence timer ends the turn.
  - `ArmTimer`/`CancelTimer` replace a single timer helper task.
  - `Interrupt` runs the barge-in path in section 3.
  - `EndUserTurn` goes through the finals gate.
- **Finals gate.**
  - `owed_finals` is +1 per VadStop forwarded and -1 per UtteranceFinal.
  - The segments belonging to one stop are joined into exactly ONE `TurnInput::SttFinal`.
  - When the machine says `EndUserTurn` while finals are owed, the end is held until they arrive or `final_wait` (config, default 1.5 s) expires. On expiry a metric is counted and the turn proceeds.
  - Finalize is sent at the VAD stop and Smart Turn runs concurrently. The added wait is therefore max(finalize RTT, Smart Turn), not the sum.
  - An empty joined transcript produces no `UserTurnEnded` downstream. The driver feeds `BotAudioEnded` itself (the caller obligation).
- **response_deadline backstop.** After `UserTurnEnded` the driver arms a deadline (default 20 s) that bounds "the bot never produced audio" (a stalled LLM or TTS). The first `TtsStarted` of that epoch (TransportOut sends it once, at the first audio handoff) cancels it, as does `TtsEnded`: a response that is playing must never hit the backstop, because the synthetic `BotAudioEnded` would make the machine Idle while the bot still talks and the user's next speech a plain new turn without `Interrupt` and `Clear`. The end of playback is then guaranteed by the transport's own mark timeout. On expiry the driver feeds a synthetic `BotAudioEnded` and counts a metric. This closes the known AwaitingResponse gap.
- **Timeline marks.** UserSpeechStart, UserSpeechEnd, SttInterim, SttFinal and TurnEnd are marked in the driver. `interrupt(old, new, at, speech_start)` is called on a barge-in.
- **Transport epoch ledger.** The ledger advances only through `Clear{epoch}`, but the turn epoch advances every turn. The rule is:
  - TransportOut keeps `tx_epoch`.
  - Before the first Audio of any epoch greater than `tx_epoch`, it sends `Clear{epoch}`. On an idle transport this cuts nothing.
  - `Clear` is idempotent for the same epoch.
  - Without this rule the second turn's audio is silently dropped.
- **Barge-in guards.** The driver's machine judges candidate barge-ins with the real `InterruptionPolicy`, configured by `EngineConfig::interruption`; see section 12, "SttStage and TurnDriver". With `InterruptionConfig::OFF` every barge-in interrupts at once (the plain turn-machine behaviour).

## 3. Barge-in path, step by step

1. VadWorker job completes and pushes `UserStartedSpeaking{at}` (system lane). STT forwards it.
2. TurnDriver feeds `VadStart`. The machine returns `Interrupt{new_epoch}` (bot active).
3. In the same `process()` call, the driver does these in order:
   1. `timeline.interrupt(...)`.
   2. FAST PATH: `CommandSender::send(TransportCommand::Clear{epoch:new})`. This is applied inline in the transport, bypasses the command queue, and the transport drops queued audio, sends Clear to the adapter, flushes the final `PlaybackPosition` and emits `MarkCleared`.
   3. Push `System Interrupt{new_epoch}` downstream.
   4. Push `Control UserTurnStarted{new}`.
4. LlmRunner receives Interrupt. It cancels the epoch token, aborts the round task and joins it with a bounded timeout. The stream and assembler are dropped. If tool calls were pending it calls `Context::interrupt_pending_tool_calls()`. If a round had streamed text but no Done, it pushes a partial assistant message with the raw text so far (a superset of what was sent). It records `pending_cut(old_epoch)`. It forwards Interrupt. It never mutates the Context while the round task is alive (join first).
5. SegmenterStage calls `reset()`. TtsStage calls `cancel.try_send(ctx)` for every open ContextId of the old epoch (priority channel) and resets the resampler. Late audio is dropped by DropStale, unknown ContextIds, and `GuardedSender`.
6. TransportOut marks `AudioOutStopped` on the interrupted epoch and sends an idempotent `Clear`.
7. At the next `UserTurnEnded` the runner applies truncation (section 5), then pushes the user message.

Target: Interrupt decision to Clear applied ≤ 1 ms (step 3.2). No stale audio reaches the wire afterwards.

## 4. LLM runner and tool loop

- `Arc<Mutex<Context>>` is a std Mutex, never held across an await. It is the probe surface for the sim.
- A single seam `RequestBuilder::build(&Context) -> providers::LlmRequest` passes the full context to the provider; the long-call context window (`build_window`) is applied there. The tool list is name-ordered for prompt-cache stability.
- A `convert` module maps agent Context/Message/ToolSpec to providers types. This covers assistant `tool_calls` with `provider_id`, arguments serialized to JSON text, and `provider_data` Value to JSON text. It also maps `LlmEvent` to `assemble::Delta`, including `Done.provider_data`.
- `provider.warm()` runs at Start.
- On `UserTurnEnded(epoch)`, in this order: apply pending truncation, push the user message, spawn ONE per-epoch round task (child CancellationToken, TaskGuard slot).
- Per round r, inside the task:
  1. Push `LlmResponseStarted{epoch}`. Register `(epoch, round_ordinal)` in the PlayoutBook with `msg_index=None`.
  2. `timeline.llm_request(epoch)` returns the round index, which is passed to `llm_first_token` and `llm_done`.
  3. `provider.stream` runs under clock-based first-token (8 s) and idle (15 s) timeouts.
  4. `TextDelta` goes to the assembler AND is emitted as `LlmText` immediately (leading whitespace of the first delta stripped once, before both). `ToolCallDelta` goes to the assembler.
  5. At Done, `assembler.finish(epoch,&ctx)` runs inline when the buffer is small (<~4 KiB) and on Offload when large. The assistant message is pushed ONCE, before execution. The PlayoutBook is updated with `msg_index`.
  6. No calls: push `LlmResponseEnded` and end the turn.
  7. With calls: push `Flush`, so the text before the tool is spoken during execution. Then race `ToolEngine::execute(calls, epoch, &token)` against `clock.sleep(delay)`, the delay of the `FillerPolicy` (`LlmConfig::filler`, default 1.2 s).
  8. If the sleep wins, emit a canned filler as its own round (own `LlmResponseStarted`). It enters the Context once no call is open (before the next assistant message, or when the turn is settled), so its `msg_index` is set then and it is cut to what was heard like any round.
  9. After execution: `Execution::append_to`, then `take_late_notes()` only when no call is open, then the next round. The limit is `max_tool_rounds` (default 6). On `budget_exhausted` or the round limit, speak a fixed fallback line.
- **Speculation is opt-in and not part of the base round loop** (see the end of section 12). Speculative tool execution is not done because it would break "only played text in context" and side-effect safety. SideEffect calls are dispatched once, after Done. The round task is never re-run after dispatch. Exactly-once comes from the ToolEngine idempotency key.
- **Errors.** One bounded retry on `LlmEvent::Error` or timeout is allowed ONLY before any text or tool dispatch. Otherwise the turn ends with `LlmResponseEnded` and no invented speech. An empty or errored response makes the runner push an upstream-safe `TtsEnded` (non-blocking), so `BotAudioEnded` fires.

## 5. Played-text truncation (marks plus released audio)

**Source of truth.**
- `PlaybackPosition` is an upper bound: audio released to the wire, ahead of the caller by sink 4 + wire queue 8. It is the continuous source.
- `MarkPlayed` is the exact boundary.
- Heard position = `min(released, total_audio) − lead`. `lead` is the client's reported playback lead when the transport sends one (`PlayoutBook::client_lead`, Twilio `PlaybackLead`; the report in force when the `Clear` cut the utterance), else `lead_margin` (configurable, default wire-queue-depth × 20 ms, and 0 in loopback tests where the oracle is wire arrival time). The audio of later utterances sits behind an earlier one in the client's buffer, so it is taken off the lead first (`RoundSnapshot::later_released`). For an interrupted response the fixed `lead_margin` (not a reported lead) shrinks by the time between the last audio released to the wire for the utterance or the ones behind it and the barge-in (`RoundSnapshot::margin_at`): after a tool or filler wait the client's buffer has drained and nothing is subtracted.
- A mark-played utterance is kept whole, and so is one followed by a later utterance whose mark was echoed. This also avoids truncating text the TTS rewrote, such as numbers. A mark echoed AFTER a `Clear` covered the utterance does not count (see "As built").
- The final position is flushed by the transport on Clear, if audio was released since its last report.

**`PlayoutBook`** (engine, std Mutex, never across an await, plus a `watch` for settle notification) is keyed explicitly by `(epoch, round_ordinal)`. Rounds are never zipped to utterances by order.
- Runner writes: `round -> msg_index: Option<usize>`.
- TtsStage writes: `round -> utterance_id`, plus the verbatim sent slices (`push_sent`).
- TransportOut and the pump write: words, positions, `marked_played`, `cleared`, `lost` and `total_audio`.
- `settled` is set when `MarkPlayed`, `MarkCleared`, or the final position after Clear arrives. While an end mark is outstanding only `MarkPlayed`/`MarkCleared` settle (the transport sends the final position before `MarkCleared`, so a stale periodic report cannot); a mid-utterance barge-in has no mark yet and takes the first position after `note_clear` as final (known limitation, see the `playout` module docs). Utterances of rounds already cut (`applied`) are ignored by the wait, and so are cut utterances of rounds without a context message (filler, fallback), which `mark_applied` never reaches. A round owns at most one utterance, and an utterance id belongs to one round; conflicting registrations are ignored and counted.
- Old epochs are evicted (bounded).

**Procedure** (runner, at `UserTurnEnded` and at Stop), before `refresh_summary` and before `build_window`:
1. Wait for `book.settled(epoch)` of the cut epoch's utterances, bounded by the clock (default 250 ms, counted from the barge-in, so nothing is waited for when the user talked on for longer). On timeout use the last known position and count `settle_timeouts`.
2. Walk the rounds from last to first, so Removed outcomes do not shift earlier indices. A round with `msg_index=None` is skipped.
3. For each round: if `marked_played` (or followed by a played utterance) and not `lost`, keep it as sent. Otherwise build `PlayedText::heard(position, &PlayedConfig)` and call `apply_truncation(&mut ctx, idx, &trunc)`.
4. A round never reached (position 0) goes through `apply_truncation` with an empty heard prefix. Whatever outcome `apply_truncation` produces is the policy, identical to the reference LlmStage. The engine adds no omission policy of its own.
5. Orphan tool results are removed together with a removed assistant message. Assistant messages that keep tool calls are retained so tool pairs stay valid.
6. Record `TruncationStats`.
7. Assert `Context::validate` after every application, in debug and in all tests.

**Sent text.** SegmenterStage uses `Segmenter::push_spans`/`finish_spans` and emits the raw slice `raw[prev_end..chunk_end]`, including its leading whitespace. The concatenation of all sent slices is then byte-identical to the assistant text, which satisfies `PlayedText::push_sent`/`apply_truncation` (no `NotAPrefix`). TTS tolerates leading spaces, and Cartesia `continue:true` wants them.

**Oracle.** The sim oracle is wire truth: the `LoopbackClient` `received_at` of each 20 ms frame, with a documented ±1 frame tolerance. `position_interval` is 20 ms in tests.

## 6. Rate conversion

- **Input.** The pump parses `Started.metadata["sample_rate"]` into typed `CallRates{input, output}` via a new `CallInfo::sample_rate()` in transport. An unknown rate is a fatal Error frame. AudioIn resamples input → 16 kHz.
- **Output.** The TtsStage reader task owns one `StreamResampler` per session, 24 kHz → `CallRates.output`. It has `flush()` at `Flushed`, `reset()` on ContextId change or Interrupt, and a reused scratch buffer.
  - Each provider chunk is copied once into a `BytesMut`, then cut into exact 20 ms `AudioFrame`s by zero-copy `Bytes::slice`. The remainder carries over to the next chunk.
  - This is one allocation per provider chunk and none per frame. It keeps interrupt granularity and PlaybackPosition accuracy at 20 ms.
  - The transport never sees a rate mismatch.
- **Measurement.** If a resampler call exceeds ~50 µs in the bench, it moves to the pool.
- **Telephony rates.** Resampling is part of the output chain. The remaining check is the 8 kHz μ-law WebSocket test with a 24 kHz TTS, which confirms that μ-law encoding lives in the adapter and not in the engine.

## 7. Error and reconnect policy

- Every network or IO await has an explicit clock-based timeout. Defaults: connect 5 s, send 2 s, reconnect backoff 1 s with a cap of 3 retries, then degrade.
- STT death: synthesize `UtteranceFinal` (the turn proceeds with what it has), and reconnect on the next turn.
- Behind the resilience layer (`brisvox_providers::resilience`, opt-in per stage) most of these
  deaths never reach the engine: the wrapper recovers in place or on a secondary provider and replays
  the open utterance (STT), re-issues a request that failed before content (LLM) or re-speaks unheard
  utterances (TTS). See "Resilience layer" at the end.
- TTS death: `mark_lost`, reconnect and `TtsEnded`. The heard prefix stops before the first untimed word.
- LLM error: see section 4.
- Pool failures (`PoolSaturated`, timeout, `JobPanicked`):
  - Drop the batch and count it.
  - If a stream was lost, rebuild it via `model.stream()` and reset the gap.
  - N consecutive failures (default 5) produce a non-fatal Error frame. The stage then fails open: it keeps forwarding audio and emits no VAD events, plus a metric.
- `Err` from `process` is reserved for unrecoverable invariants. Fatal ErrorFrames go to the supervisor, and a non-fatal Error goes to a counter plus tracing target `brisvox::engine`.
- Transport `Stopped`: TurnMachine `Hangup`, cancel the round and tool tokens, `running.stop(timeout)`. Dropping the call future aborts every TaskGuard, so no tasks are left behind.
- One session's panic stays inside its own tasks. The only shared object across sessions is the bounded inference pool.
  - The pool has two lines (`brisvox_inference::Priority`): the VAD batch of every call goes in the `High` line, everything else (Smart Turn analyses, warm-ups, builds) in the `Normal` one; a worker takes a waiting `High` job first, and each line is bounded by the queue capacity on its own. Under overload the Smart Turn analyses wait (and fail by their own timeout, which the turn machine handles as a missing verdict) before a VAD window is lost. The queue capacity must cover one job per call in each line (`brisvox-server` sizes it `max(1024, 2 x max_sessions)`).
  - Smart Turn analyses have an executor of their own (`SessionBuilder::with_smart_turn_offload`, `brisvox_engine::offload::start_smart_turn_executor`): a second bounded pool (32 jobs per thread, reject newest) with the same timeout and failure handling, so a 20-25 ms analysis can never hold the thread a VAD window needs. The model's warm-up sizes itself on the executor's threads. Without `with_smart_turn_offload` (and with `BRISVOX_SMART_TURN_THREADS=0`) the analyses use the session's offload as before.

## 8. Latency design and budget

- **Prewarm at Start:** `provider.warm()`, STT and TTS connect, and a Smart Turn warm per pool thread.
- **Parallel work:** Finalize at VAD stop runs concurrently with Smart Turn.
- **Audio path:** forwarding never waits on the pool (VAD tee). The tee copies about 640 B per frame into recycled buffers: zero allocations in steady state, at the price of a small memcpy.
- **Clear:** fast path, directly to `CommandSender`.
- **Segmenter:** first clause after 2–3 words. As built: after **one** word in
  English and Turkish. English is decided from measurements (`docs/LATENCY.md`, "First TTS
  chunk"); Turkish follows it as an assumption, not yet measured.

Engine-added overhead budget, excluding VAD silence, model and provider time:
- gated turn end → LLM request issued: ≤ 3 ms p95
- first TTS audio byte → transport command: ≤ 2 ms p95
- Interrupt decision → Clear applied: ≤ 1 ms

These are measured in the V2V bench (`brisvox-bench v2v --sim`, the engine's own timeline:
`docs/LATENCY.md`, "Engine stage times and memory"); all three are met.

## 9. Test strategy: sim suite against the production pipeline

- **Seam.** `brisvox-sim` gains `trait SessionFactory { build(SessionParts) -> BuiltSession{pipeline, probes, InputSink, OutputLog} }`. `ReferenceFactory` reproduces today's chain exactly. Scenarios s01–s11, played_text, fixtures and chaos bodies move into `brisvox_sim::suite`, generic over the factory and a `Profile{Exact, Budget}`. Existing sim tests must keep byte-identical results: fingerprints are checked before and after the refactor.
- **Profiles.**
  - Structural invariants are identical under both profiles and asserted exactly: outcome Ok; determinism (two production runs have identical fingerprints); no stale audio after an interrupt; context == played prefix; roles and turn counts; interrupt counts (0 for s06 and s09, 1 for s02 and s10); recovery; isolation versus solo; no leaked tasks (`TaskCounter`); heap-flat.
  - Exact timings (1480 ms, 280 ms, 660 ms) stay on the reference. Production uses a documented Budget table with ranges. Fixtures reach first audio within 600 ms of utterance end, and the chaos SLO is 1.5 s.
  - Production fingerprints are compared only to production fingerprints.
- **Interrupt counts** come from `Interrupt` frames (a test-only recording tap before TransportOut), NOT from client Clear events, because Clear fires on epoch bumps too.
- **Hybrid chains** prove each production group against the untouched reference stages, each gate running s01–s05 or s01–s11 structural invariants under a paused SimClock:
  1. Engine pump/AudioIn/VAD + reference STT/Turn/LLM/TTS/pacer.
  2. Engine input chain + STT + TurnDriver + reference downstream.
  3. Engine input chain + LlmRunner + reference output.
  4. Everything in engine.
- **Harness.** Input in tests comes from `LoopbackClient::send_audio` through the real TransportSession and pump. `LoopbackClient` gets an auto-ack-marks option (configurable delay and a never-ack mode). `LoopbackConfig.latency=0`.
- **Dedicated tests.** The following each need their own test:
  - Two consecutive non-barge-in turns both reach the wire.
  - Races: a verdict after a new VadStart, and `BotAudioEnded` after Interrupt.
  - Finalize never precedes speech audio, with a slow STT inbox.
  - Tool exactly-once with a barge-in during execution, then a new turn: body counter == 1, late note exactly once.
  - Multi-round turns (text, tool, text) with a barge-in in round 2.
  - Settle-timeout fallback.
  - A missing final `PlaybackPosition`.
- **Hot path.** A counting-allocator test asserts 0 allocations per steady-state frame on AudioIn → STT → TurnDriver forward, and on the TTS slice path (per-chunk allocation is documented). A multi-thread real-time test (SystemClock, real pool, silero-tract, 20 sessions) has a scheduler-gap probe asserting no tokio worker stall over 2 ms. Micro-benches of `Segmenter::push`, `ResponseAssembler::push/finish`, `TurnMachine::handle` and the resamplers are held to the 50 µs rule.
- **Chaos.** 100 production calls cover all chaos kinds plus HangUp. SlowTransport is a slow `LoopbackClient` consumer.

## 10. Build order

The pipeline was built in ten stages: the simulation seam (session factory, input sink, output log,
shared suite with exact and budget profiles), the core self-wake and engine skeleton, the input
chain, the STT stage and turn driver, the LLM runner, the output chain, played-text truncation from
the play-out book, assembly (`run_call` / `build_session_pipeline`), chaos / isolation / leak /
allocation / blocking verification, and latency tuning. Section 12 records what was built per
component.

## 11. Main risks

1. **Wake/EventSlot self-wake touches core.** Mitigations: core tests first, the 64-byte assert, and `Custom` as the documented fallback.
2. **PlaybackPosition is an upper bound.** Real adapters may over-include the last words. Mitigations: marks, `lead_margin`, and the settle wait. The sim cannot prove real-network accuracy.
3. **Epoch/Clear coupling.** A missed lazy Clear leaves the bot silent without any error. The output chain has a dedicated two-turn test and a counter for transport-dropped stale frames.
4. **Pool versus paused time.** Inline offload can mask pool-only bugs (state loss, job ordering). Only the real-time test and the bench cover these.
5. **Finals gate** is new logic beyond the reference. A final that never arrives misaligns the machine. Mitigated by the one-per-Finalize guarantee, `final_wait`, and driver-side counting.
6. **Budget profile loosening.** Budget ranges must be set from measured values and reviewed. Never compare production fingerprints to the reference.
7. **Scope.** The turn driver, the LLM runner and the output chain are the heaviest parts.

## 12. As built: the input chain

Code: `crates/brisvox-engine/src/{pump,audio_in,vad}.rs`. Details live in the module docs; the
decisions that go beyond sections 1 to 3 and 7:

- **Pump** (`Pump::spawn(session, PumpLinks{input, upstream}, book, clock, config, cancel) ->
  PumpHandle`). It owns the `TransportSession` and is started after the pipeline (it needs
  `RunningPipeline::input()` and `upstream_input()`). Audio is `try_send` only. Lossless frames
  (DTMF, `Stop`, a fatal `Error`, the upstream `TtsEnded`) go through bounded FIFOs, one per
  target lane (data, system, upstream), each delivered by its own `select!` branch, so a stuck
  pipeline never stops the reading and a frame stuck on one lane never holds back another. The
  `Stop` therefore overtakes a key press that is stuck on a full data lane (hangup beats the last
  digit; the digit is still offered until delivered or the flush gives up). Hangup is
  `SystemFrame::Stop` into the first stage plus `PumpHandle::end()`; a hangup is also issued when
  the event stream ends without `Stopped` (a defensive branch: `TransportSession` always sends
  `Stopped` unless it is lost to a full queue, which no test can provoke through the public
  session, so that branch is untested), and the FIFO is flushed (bounded by `flush_timeout`)
  after the session closes (tested: `the_pending_stop_is_flushed_after_the_session_closed`). After
  a hangup audio and DTMF are discarded and counted while marks and positions still reach the book
  (tested: `after_a_hangup_nothing_reaches_the_pipeline_but_marks_still_reach_the_book`). The
  call's rates are `PumpHandle::rates()`.
  `TransportEvent::Error` is a NON-FATAL transport fault (the Twilio connection keeps its own
  fault budget and ends the call with `Stopped` when it is spent), so by default the pump only
  counts it and logs a warning (`hangup_on_error`, default OFF; review fix, it used to hang up on
  the first fault and defeated the transport's budget). `hangup_on_error: true` restores the strict
  reading where the first `Error` hangs up.
- **Epoch of user-side frames.** The pump tags audio and DTMF with epoch zero. The TurnDriver
 re-tags DTMF with the current epoch before it goes to the bot side.
- **End mark.** `PlayoutBook::mark_played_resolved` returns the utterance's epoch, so the upstream
  `TtsEnded` carries it. A cut mark (`MarkCleared`) updates the book and ends nothing upstream
  (the barge-in's `Interrupt` already told the machine).
- **AudioIn** forwards a 16 kHz frame untouched (same `Bytes`), converts other rates through a
  `StreamResampler` into a ring of 32 reusable `BytesMut` slots (no allocation per frame once the
  ring has been used once), copies the 16 kHz samples into the `VadTee`, and never waits for the
  worker. A jump in `AudioFrame::seq` (frames dropped by the first-stage inbox) resets the
  resampler and marks the tee frame as following a gap.
- **VadTee**: bounded (the input queue's data capacity, 8), drop-oldest, copies samples into
  recycled buffers (so it does not depend on how the transport allocated its `Bytes`), wakes the
  worker with a `Notify` permit.
- **Gap flags and timestamps** (review fixes). A seq jump, a frame lost in conversion, or a
  resampler error sets a pending gap that is carried to the next tee frame actually pushed (a
  frame without resampled output cannot lose it). Resampled output is stamped with the time of
  its first sample (completing frame's timestamp, plus samples produced, minus samples consumed,
  minus `StreamResampler::delay()`), not with the completing input frame. A rate change resets
  the resampler even when the frames in between were at the model rate.
- **VadWorker**: takes everything queued as one batch, one job in flight on the `Offload`; a batch
  is split into contiguous segments and the stream is closed (`VadStream::finish`) at every gap, so
  speech in progress gets a stop dated to the last speech. A job the pool refused (saturated) or timed
  out before a pool thread started it never ran, so nothing is lost: the stream and
  batch wait in a slot the job takes them from, the worker takes them back, waits 10 ms on the
  pool's clock and offers the same audio again with what arrived meanwhile (`retries`; kept audio
  bounded by max(tee capacity, 50 frames), beyond that shed like a tee overflow). Overload becomes
  delay, not a lost turn. A job that started and was lost (panic, timeout while running, closed
  pool) loses the stream: a stop is synthesized if speech was in progress and the stream is
  rebuilt from the setup's model factory. `max_consecutive_failures` (default 5) failed batches
  in a row that also span at least `failure_window` (default 1 s of audio time, from the batch
  timestamps) fail the worker open: one non-fatal `Error` frame, no VAD events, the tee is only
  drained. Fail-open is NOT permanent (review fix): every `recovery_interval` of audio time
  (default 5 s, `None` disables) the next batch is run as a probe through a fresh stream; the
  first success clears the failed-open flag and the streak (`recoveries` counter, a warn log, no
  frame) and events resume, a failed probe keeps the worker failed open (speech it started is
  closed again) until the next interval. Batches come every 20 ms, so a
  count alone would let a 100 ms pool-saturation burst end VAD for the call; a burst shorter than
  the window heals with the next successful job. `EnergyModel` (320-sample windows, RMS threshold) lets paused-time simulations run
  this exact path; with the simulator's thresholds the hybrid marks speech at the same instants
  as the reference energy VAD.
- **Tests**: `tests/{pump,audio_in,audio_in_alloc,vad_worker,hybrid_input}.rs` (paused time and
  real-time pool tests) and `tests/silero_realtime.rs` (feature `silero-tract`).

### SttStage and TurnDriver

Code: `crates/brisvox-engine/src/{stt,turn_driver,timer}.rs`; the module docs hold the details, the
decisions that go beyond sections 1 to 3 and 7:

- **SttStage** (`SttStage::new(provider, SttStageConfig::from_engine(&config), clock)`). `process`
  never awaits the provider. Audio is forwarded downstream at once and copied (a refcounted `Bytes`)
  into a bounded command queue (mutex-guarded ring plus a `Notify` permit: audio drop-newest at 64,
  `Finalize` at 32, overflow answered synthetically). ONE session task owns the provider session,
  the ledger of unanswered `Finalize`s and every frame the stage emits besides the forwarded ones,
  so transcripts and `UtteranceFinal`s are strictly ordered. The task connects when the pipeline
  starts (prewarm; explicit `connect` timeout on the clock) and reconnects after a failed connect or
  a dead session with a fixed backoff, at most `max_reconnects` consecutive attempts; then the stage
  is degraded until the next `UserStartedSpeaking` grants a fresh budget. A successful connect does
  not reset the budget (a provider that accepts the socket and drops it would be retried for ever);
  the budget is restored when the session delivers a final segment or lives at least 10 s. No
  session means audio is dropped (counted) and every `Finalize` is answered synthetically at once.
  A synthetic answer keeps the words the provider had heard: the latest interim since the last final
  segment is forwarded as a final segment just before the synthetic `UtteranceFinal`
  (`interims_promoted`). A real answer to a `Finalize` does the same when no final segment was
  forwarded since the last `UtteranceFinal` (a session that died mid-utterance leaves its interim
  behind and its successor answers the `Finalize` empty); otherwise the caller's words would be
  lost. Upstream frames are forwarded with `try_send` (drops counted), never
  awaited from `process`.
- **Exactly one `UtteranceFinal` per `Finalize`.** Only `from_finalize && speech_final` answers
  the oldest outstanding `Finalize` (a `speech_final` from the provider's own endpointing closes
  nothing; a `from_finalize` result with nothing outstanding is late and dropped). A `Finalize`
  unanswered after `Timeouts::finalize` (3 s, longer than the driver's `final_wait`) declares the
  session dead: every outstanding `Finalize` gets its synthetic answer and the session is replaced,
  so the provider's late answer can never be seen (no per-`Finalize` ids exist in the protocol, so a
  kept session could not tell a late answer from the next one).
- **`Finalize` never precedes the speech it finalizes.** The VAD stop is a system frame and
  overtakes queued audio. A `Finalize` is therefore held until the stage has processed an audio
  frame stamped at or after the (backdated) stop time, or `max_finalize_defer` (1 s) passed, or the
  pipeline stops. Normally the frame has long been processed and nothing waits. The 1 s cap is a
  clock deadline (the stage's own `DeadlineTimer`, woken through `SystemFrame::Wake`), so it holds
  even when no further audio arrives (a leg that suppresses silence, a stalled lane): the
  `Finalize` is then sent after at most 1 s instead of waiting for the driver's `final_wait`.
- **TurnDriver** (`TurnDriver::new(TurnDriverConfig::from_engine(&config), clock, timeline)`,
  builders `with_smart_turn(model, offload)`, `with_commands(CommandSender)`,
  `with_interruption(policy)`, `with_probe(TurnProbe)`). One task; helpers (the single
  `DeadlineTimer` for the machine's, the gate's and the response backstop's deadlines, and the
  Smart Turn job) return through the `EventSlot` and a `Wake`. Handling is level-triggered: on any
  wake it compares every deadline with `clock.now()`.
- **Smart Turn.** The driver owns the 8 s ring (`SmartTurn::push` of the forwarded audio, no
  allocation per frame; 128 000 samples, 256 KB; `SmartTurn::new` allocates only the ring, the
  buffer of its own `analyze` path is lazy and the driver never calls `analyze`).
  `RequestAnalysis` copies the ring with `SmartTurn::snapshot_into` into an owned `Vec<i16>` and
  runs `SmartTurn::analyze_snapshot_blocking` inside `Offload::run`, so the ring keeps filling.
  The job hands the buffer back with its verdict (`Event::Verdict::samples`) and the driver reuses
  it for the next pause: only the first pause of a session allocates 256 KB on the async worker
  (a session that has paused holds 512 KB: ring plus recycled buffer; a timed-out job loses its
  buffer and the next pause allocates again). Measured in release
  (`cargo test -p brisvox-inference --release --lib the_caller_side_copy_is_cheap -- --nocapture`,
  medians of 21 runs, 8 s of audio, 2026-10-08): 1.8 us into a reused buffer, 2.5 us into a fresh
  allocation (a warm allocator; a cold page fault can cost more, which is why only the first
  pause allocates). The reused path is far inside the 50 us worker budget of CONTRIBUTING.md. The original plan named `analyze_snapshot(offload, samples, timeout)`;
  the delivered async `analyze_snapshot(&PoolHandle, model, samples, timeout)` takes the pool handle
  because `brisvox-inference` cannot depend on the engine's `Offload` (the engine would be a
  dependency cycle), and the engine, which must also run under `Offload::Inline` in paused-time
  simulations, calls `analyze_snapshot_blocking` itself; the async variant is for pool-only callers.
  A failed, timed-out or unavailable analysis is counted and the machine's `fallback_silence`
  deadline ends the turn. `brisvox-inference` gained `snapshot`, `snapshot_into`, `model`,
  `analyze_snapshot` (pool) and `analyze_snapshot_blocking` (additive).
  The VAD stop travels the system lane and can overtake caller audio queued in the driver's data
  lane, the same hazard the STT stage defers `Finalize` for. While audio is flowing but the newest
  ring frame is stamped before the stop, the analysis is deferred (`analyses_deferred`) until a
  frame stamped at or after the stop is in the ring, or at most `ANALYSIS_DEFER_MAX` (200 ms) on
  the driver's timer (a silence-suppressing leg sends no later frame); a resume, turn end or
  hang-up drops it. Before any audio has arrived there is nothing to lag behind and no deferral.
- **Barge-in.** `Interrupt` does `timeline.interrupt`, then the transport `Clear{new_epoch}` through
  the `CommandSender` (or a `ClearHook`, `with_clear_hook`; bounded by `Timeouts::send`; a failed or
  timed-out clear is counted and the `Interrupt` still follows), then pushes `SystemFrame::Interrupt`, then the
  next action pushes `UserTurnStarted`. STT interims are fed as `SttInterim` (and not forwarded);
  upstream `TtsStarted` / `TtsEnded` become `BotAudioStarted` / `BotAudioEnded`, which open and close
  the echo window. `EngineConfig::interruption` is a `Copy` description (`InterruptionConfig`,
  `OFF` or `RECOMMENDED`, default `RECOMMENDED`) of the agent's `InterruptionPolicy`; custom phrase
  lists go through `with_interruption`.
- **Finals gate.** The driver keeps one `OwedStop` per `UserStoppedSpeaking` (with the turn it
  belongs to; `None` for a vetoed candidate's stop). The final segments between two
  `UtteranceFinal`s are joined into one `SttFinal` for the machine and appended to the turn's text.
    A machine `EndUserTurn` is held while a stop of that turn is unanswered, until `final_wait`.
  When `final_wait` expires and the turn has no text at all (no final text of an answered stop
  and no interim), the hold is extended ONCE by `TurnDriverConfig::late_final_wait` (default
  `timeouts.finalize - final_wait`, 1.5 s; zero disables; counted as `gate_extensions`): the STT
  stage answers every stop itself at `Timeouts::finalize`, and a final that is only a little late
  would otherwise be dropped (`late_finals`) and the caller left unanswered. A turn with text, or
  with an interim to fall back on, is not extended. Release pushes the joined `Transcript` and `UserTurnEnded`, marks `TurnEnd` and clears the Smart
  Turn ring. A gate timeout uses the latest interim if no final text arrived; an empty text emits no
    `UserTurnEnded` and feeds the machine a `BotAudioEnded`. If the user starts speaking while an
  end is held, it is a RESUME, not a barge-in: the machine is already in `AwaitingResponse`, but
  no response was requested and no bot audio exists, so the driver first feeds it the same
  synthesized `BotAudioEnded` as for an empty turn and then the `VadStart`, which opens a new turn
  without any guard (no minimum speech, no backchannel veto, no `Interrupt`, no transport `Clear`).
  The held end is dropped, its text carried into the new turn and its owed finals re-attributed
  (the user is answered once, after the whole utterance). A gate timeout never replaces the
  confirmed text of an earlier stop with a later, unconfirmed interim.
- **Backstop and hang-up.** After `UserTurnEnded` a `response_deadline` (cancelled by the first
  `TtsStarted` of that epoch, and by its `TtsEnded`; see section 2) feeds a synthetic
  `BotAudioEnded` when the bot never produces audio. `SystemFrame::Stop`
  closes the machine, drops an open turn and a held end without `UserTurnEnded`, cancels the timer
  and any analysis, and is forwarded. Caller audio, VAD events and transcripts stop here; DTMF is
  re-tagged with the machine's epoch.
- **Timeline.** `UserSpeechStart` (at the VAD start), `UserSpeechEnd` (at the backdated stop of a
  stop the machine took into a turn), `SttInterim`, `SttFinal` (when the turn's stop is answered),
  `TurnEnd` (when the gated end is released, so STT wait counts as end-of-turn delay) and
  `Interrupted` via `timeline.interrupt`.
- **Config rule.** `EngineConfig::validate` requires `timeouts.finalize > final_wait` (the driver
  gives up on a final before the STT session is restarted).
- **Tests.** `tests/stt_stage.rs` (MockStt faults, slow inbox, held-finalize deadline, queue
  overflows, reconnect budget, promoted interims including a real empty answer after a
  mid-utterance session death, non-blocking upstream, stuck provider, late answers), `tests/turn_driver.rs` (policies, gate, barge-in with a real `TransportSession` clear,
  failing and hanging clears and analyses, exact Smart Turn ring content and clearing, a stop that
    overtakes queued audio, guards (the echo window in isolation, with a control run), resume while
  an end is held, the extended gate, the owed-ledger and text bounds, non-blocking upstream
  forwarding, `with_interruption`, `Cancel`, hang-up, backstop (also `TtsEnded` alone),
  timeline marks of a held and a lost-final turn, determinism including the exact timer
  deadlines, and a table of 18 scenarios, including stale verdicts, `final_grace` expiry,
  barge-in while awaiting the response, stale and repeated bot audio and empty turns, replayed
  through the driver and required to give the bare `TurnMachine`'s exact action sequence), `tests/hybrid_turn.rs` (gate 2: pump,
  `AudioIn`, VAD worker, `SttStage` and `TurnDriver` in front of the reference downstream; s01..s11 and
  the basic suite under Budget with fixed silence and with the adaptive policy plus a fixed mock
  model, with the recommended guards and with every guard off).
- **Not covered in the sim.** The hybrid output is the reference stage, which reports only
  `TtsEnded` upstream, so the echo window is exercised by `tests/turn_driver.rs` only; the real
  `TtsStarted` comes with `TransportOut`.

### LlmRunner, request building and the tool loop

Code: `crates/brisvox-engine/src/{llm,llm_runner,llm_turn,convert,request,truncation}.rs`; the module
docs of `llm.rs` hold the full protocol. The decisions that go beyond sections 1 to 4:

- **Shape.** `LlmRunner::builder(provider, &config, clock, timeline)` (system prompt, tools +
  engine, flow, summarizer, playout book, truncation source, offload, canned phrases) `.build()`.
  The runner is a `DropStale` processor; `UserTurnEnded` spawns ONE round task per turn (a
  `TaskGuard` and a child `CancellationToken`) that owns all rounds of the turn. `Transcript`
  (final) is consumed, `UserTurnEnded` is forwarded first, `Start` warms the provider in a bounded
  helper task, upstream frames are forwarded with `try_send`.
- **Round ordinals.** `LlmResponseStarted` is sent per round (filler and fallback rounds
  included); the ordinal of a round is the number of `Started` frames of its epoch before it. The
  runner registers `(epoch, ordinal)` in the `PlayoutBook` BEFORE sending the frame and records
  `msg_index` when it pushes the assistant message. The timeline round index (`llm_request` /
  `llm_first_token` / `llm_done`) counts model requests only and is separate.
- **Streaming.** The first-token timeout (`Timeouts::llm_first_token`) covers the request and the
  first event; `llm_idle` bounds every later event; both on the injected clock. `LlmText` goes out
  per delta (the first delta of a round is left-stripped once, whitespace-only deltas are skipped
  until there is text); the raw text so far is recorded BEFORE it is sent, so a partial message is
  always a superset of what was sent. Per delta cost: one `Arc` clone, one mutex lock, one
  `push_str`; no other allocation.
- **Finish.** `ResponseAssembler::finish` runs inline below `LlmConfig::inline_finish_bytes`
  (4 KiB) and on the `Offload` above it. The assembler mints ids from a proxy context holding the
  calls already used in the epoch (with a flow: the manager's offset-aware count, because a reset
  removes calls from the context but their ids must not repeat for the engine's idempotency key).
  A failed offloaded finish (saturated, timeout, panic) ends the round with its text only.
- **Tool loop.** `Flush` before execution (only if text was sent since the last flush; the same
  rule holds before ANY speech that is not the model's response, so the over-limit round's text
  and the fallback line are separate utterances with their own ordinals);
  `ToolEngine::execute` is raced against the filler delay of the `FillerPolicy` (from
  `LlmConfig::filler`: by default 1.2 s, or the smallest `filler_after` of the called tools under
  `FillerConfig::PER_TOOL`); an `Execution` the runner discards undelivered reports its SideEffect
  results as late notes and orphans the calls (`docs/TOOLS.md`); the
  filler is its own round, put into the context once no call is open (before the next assistant
  message, or when the turn is settled) and cut to what was heard; results via `Execution::append_to` (or the flow's
  `apply_results`); late notes are taken only while no call is open, at the start of a turn
  (before the user message) and after each round's results. `max_tool_rounds` (6) rounds execute
  tools; the next response's calls are answered `tool_round_limit` without running, and the
  fallback line is spoken and put in the context (a real utterance, cut like any other). The
  engine's own retry budget ends the loop the same way. Calls with no tool engine configured are
  answered `unknown_tool`. SideEffect and ReadOnly calls are both non-speculative.
- **Barge-in / new turn.** `halt`: read the barge-in instant FIRST, cancel the token, abort, join
  (bounded by `Timeouts::join`); a task that does not stop leaves the context untouched
  (`mutations_skipped`), is kept as a zombie that every later halt joins again before it touches
  the context, and the turn that triggered the halt is dropped (its transcript is lost by
  design: the caller's next utterance starts a new turn; the machine is told upstream). A halted
  task also never writes the context itself: its token is checked before each mutation. After
  the join: partial assistant message from the raw text (if a response was streaming),
  `interrupt_pending_tool_calls`, and every unapplied round is stamped with the barge-in instant
  taken at the start of the halt. A new `UserTurnEnded` while a task is alive is handled the same
  way without the stamp (`a_new_turn_while_a_round_is_streaming_halts_it_without_a_barge_in_stamp`;
  `Stop` during a streaming round: `a_stop_while_a_round_is_streaming_...`). A PANIC in the round task is contained (`round_panics`): the turn is
  closed like a failed one (`LlmResponseEnded`, upstream `TtsEnded` if silent) instead of leaving
  the machine to its 20 s response deadline.
- **Errors.** Retries (`LlmConfig::retries`, default 1, `retry_delay` 200 ms) only while nothing
  of the response was sent. After the final failure: partial text kept, `LlmResponseEnded`, and
  an upstream `TtsEnded` (non-blocking) only if the whole turn produced no text (otherwise the TTS
  chain ends the response itself and an early `TtsEnded` would end the bot state under playing
  audio). The suite's s06 scripts a failing response followed by the NEXT turn's answer, so the
  hybrid test sets `retries = 0`; with the default a retry would consume the next script. A
  gate-3 pass therefore does NOT cover s06 under the default retry configuration (the retry is
  tested in `llm_runner.rs`); a second hybrid test runs every other scenario with the default
  (`every_scenario_but_s06_passes_with_the_default_retry_configuration`). The played-text suite
  runs with a 300 ms fixed silence ("Quick") instead of the 450 ms of s01..s11 because its
  barge-ins come 0.4 s after the first utterance. the production pipeline must not inherit either accommodation
  silently.
- **Truncation.** `TruncationSource::cut(&CutRequest{key, sent, interrupted_at}) -> Cut`
  (`Keep`, `Heard(Truncation)` counted, `HeardInFull(Truncation)` applied but not counted) is
  asked last round first at `UserTurnEnded` (before the user message) and at `Stop`; the stub is
  `KeepAll`. The production source implements it over the `PlayoutBook` (async: the settle wait); the hybrid
  test implements it over the reference word and playout logs, judged at `interrupted_at`. The
  runner bounds the whole pass by `Timeouts::analysis_job` on the injected clock (a source that
  never answers is answered with `Keep` after it, `cut_timeouts`); the production settle wait must
  therefore fit inside that bound.
- **Flow hook.** `with_flow(FlowManager)` starts the flow on the context (node prompt = message
  0), offers `flow.tool_specs()` per round, screens the calls, applies the results each round
  (`apply_results` under the context lock; a decided transition is entered on a COPY of the
  context because the summarizer awaits, then swapped in; a task aborted in between leaves the
  decision pending and the next turn finishes it). `context_replaced` drops the round records,
  marks the book's rounds applied and forgets the window summary. `Say` actions are not played yet.
- **Request building.** `RequestBuilder::build` is the single seam: `build_window` with
  `EngineConfig::llm.window`, then (if the latest turn alone is over budget) the tool results of
  that turn are clipped oldest first to `tool_result_floor_tokens`, then the provider conversion
  and the name-ordered tools. A summarizer keeps the summary of dropped turns current, ENTIRELY
  OFF the request path: a request never waits for it (the budget of section 8, "gated turn end ->
  LLM request issued <= 3 ms", would not survive a summarizer round trip, and with a saturated
  window every turn needs a new summary). After `LlmResponseEnded` the runner starts ONE
  background task (owned by the runner, cancelled with it, bounded by `summary_timeout` = 8 s)
  that refreshes the summary AHEAD: it summarizes the window as it will be once the next user
  message is in (a placeholder user turn the size of the last one), so in steady state the summary
  is ready when the next turn needs it. The ahead refresh NEVER covers the turn that has just
  finished (`refresh_summary_until`, limit = start of the last real user turn): that turn's last
  assistant message is not final until the played-text cut ran (a barge-in right after the
  response changes or removes it, so a summary of it would break "the LLM context holds only what
  was played"), and late notes and interrupted-call results are still appended to it (a
  `covers_until` at the end of the context would stop being a turn start and the whole summary
  would be dropped as `InvalidSummary`). The ahead summary therefore always ends at a turn start
  that later changes cannot move. Conservative trade-off, chosen deliberately: when the
  just-finished turn ALONE is too large for the window (its tool results exceed the budget), the
  next request goes out without that turn (counted: `uncovered_requests`) and the following
  refresh covers it. A request that finds uncovered dropped turns for any other reason (quick
  turns, a slow summarizer, a bigger user message) goes out with the summary at hand and is
  counted likewise. A summary computed for a context that a flow Reset replaced meanwhile is
  discarded (`summaries_discarded`); the generation check and the store are one step under the
  summary mutex and `reset_summary` bumps the generation and clears the summary under the same
  mutex, so a refresh cannot store the old conversation's summary after the reset. A too-large or failing summarizer never replaces the summary;
  each failure is counted (`summary_too_large`, `summary_failures`, `summary_timeouts`,
  `consecutive_summary_failures`, `summary_degraded` after 3) and logged at `warn`; once degraded
  a circuit breaker backs the attempts off (`summary_timeout` doubled per further failure, at most
  60 s; `summary_backoffs`), so a hanging summarizer costs a bounded number of billed calls.
  `Done.provider_data` counts against the budget (`WindowEstimator`) and is bounded: dropped
  (`provider_data_oversized`) above 32 KiB or above half a message's allowance
  (`max_message_tokens * 2` bytes), parsed on the offload from `inline_finish_bytes`.
  The simulator's reference `LlmStage` is deliberately unchanged (the golden baseline); the
  context windowing lives in the engine `RequestBuilder`.
- **Cost of a request build (50 us rule).** `build` walks the full transcript once for the cut
  (turn starts, validation: a few ns per message; the transcript is never trimmed) and clips only
  the messages of the window. `window::clipped` gallops up from a 256-byte prefix and then
  bisects, so clipping costs about 10 times the SIZE OF THE RESULT (a 4 MiB tool result clipped to
  250 tokens looks at about 10 KB; plain bisection from the full length looked at 8 MiB), not
  O(size of the text) per step. Measured (`tests/llm_window.rs`
  `building_a_tool_heavy_request_is_cheap_...`, Windows dev machine): a request with 12 tool
  results in one turn, over budget, costs about 0.2 ms in a debug build and 0.1 ms in release
  whether the results are 20 KB or 60 KB each, i.e. above the 50 us rule, so it is NOT built on
  the worker when it can be that heavy: `RequestBuilder::build_offloaded`
  (what the round task calls) builds on the `Offload` when the context holds more than
  `INLINE_MAX_MESSAGES` (384) messages or the latest turn more than `INLINE_MAX_TURN_BYTES`
  (16 KiB) of text (`is_heavy`, O(latest turn)); the summary refresh does the same for its context
  snapshot (the deep clone of the transcript) and the flow transition for its context copy. Below
  those sizes a build is a handful of small clips and conversions. A pool that refuses or times
  out the job falls back to building inline (`offload_fallbacks`): a request must not fail on a
  busy pool. Counters: `builds_offloaded`, `snapshots_offloaded`, `offload_fallbacks`. In a long
  call (more than 384 messages) every request therefore pays one pool round trip; a lone VAD job
  ahead of it in the queue is the price. Thresholds chosen conservatively; just below them
  (380 short messages, or a latest turn of four 3.7 KB results) a build costs 17-19 us in a debug
  build and about 3 us in release (same test), inside the rule with a wide margin. The
  deterministic guard against a return to bisecting the whole text is the unit test
  `clipping_a_huge_message_costs_in_proportion_to_the_limit_not_to_the_text` (brisvox-agent); the
  timing test only guards coarsely (20 ms per heavy build, and no growth with the result size).
  A micro-bench of `build_window` repeats the measurement under load.
- **Filler.** The filler is sent while `ToolEngine::execute` is still polled (both futures are
  driven together), so a back-pressured pipeline cannot stall a tool or its timeout accounting.
- **Additive change in brisvox-agent.** `TokenEstimator: Send + Sync` (the window refresh holds a
  `&dyn TokenEstimator` across the summarizer's await, which must keep the round task `Send`).
- **Tests.** `src/{convert,request}.rs` unit tests (round trips preserving `Context::validate`,
  clipping, summaries); `tests/llm_runner.rs` (turn, filler, limits, errors, barge-in, exactly-once
  SideEffect with a late note, truncation order, offload on a real pool, flow);
  `tests/llm_window.rs` (a 60-minute call asserts the size of EVERY request, with a turn cap
  (50) far above what the 4000-token budget allows, so only the budget can produce the bound; 12
  large tool results in one turn; a summarizer returning too-large summaries; a 1 s summarizer,
  quick turns and a summarizer that never answers never delay a request, all with a window whose
  budget binds before the turn cap; the cost of a tool-heavy build does not follow the size of
  the results; builds on a real pool and a refused pool); `tests/hybrid_llm.rs` (gate 3: engine
  input chain + STT + TurnDriver + LlmRunner in front of the reference TTS/pacer/output; s01..s11,
  with and without a registered tool, the basic suite, fixtures, the 40 random barge-ins of the
  played-text suite, leak check).
- **Not covered here.** The `PlayoutBook` as truncation source and the settle wait;
  `Say` actions and `apply_recorded_result` for late flow results; speculative tool starts; the
  real `TtsStage` consuming the ordinals (covered in the output chain section below).

### Output chain: SegmenterStage, TtsStage, TransportOut

Code: `crates/brisvox-engine/src/{segmenter,tts,tts_audio,transport_out,rates}.rs` and
`Segmenter::push_spans` / `finish_spans` in `crates/brisvox-agent/src/segment.rs`. The module docs
hold the details; the decisions that go beyond sections 1, 5, 6, 7 and 9 (deviations from the text above: `TransportOut` is `KeepAll`, the TTS stage and not the segmenter stage records the sent slices):

- **Sent text is a prefix of the message, by construction.** `Segmenter::push_spans` returns each
  chunk with its byte range over the accumulated stream (`Span`; ranges are kept per char, so
  whitespace the segmenter drops beyond its run cap still lies inside a raw slice). The
  `SegmenterStage` emits the raw slice `stream[prev_end..chunk_end]` (leading whitespace
  included, which Cartesia's `continue: true` wants), keeps only the unemitted tail of the stream,
  and starts a new stream (offsets from zero) at every `Flush`, `LlmResponseEnded`,
  `LlmResponseStarted` and `Interrupt`. Property tests (`tests/segment_spans.rs`, EN and TR, trailing
  whitespace, tiny `max_chunk_chars`, random deltas) prove that the slices concatenate to the
  text with its trailing whitespace trimmed, and that `PlayedText::push_sent` +
  `apply_truncation` never fail with `NotAPrefix` at any playback position; the same holds
  through the stage (`tests/segmenter_stage.rs`). `first_min_words` is per language
  (`SegmenterStageConfig::new`): English 3, Turkish 2 (a Turkish word carries more); a starting
  value, not measured. **Memory bound:** the stage keeps the raw stream text it has not cut yet;
  whitespace the segmenter shortens would otherwise stay there for ever. Past
  `RAW_CAP_FACTOR` x `max_chunk_chars` bytes (16,000 with the defaults) the stage ends the stream as
  a `Flush` would (`overflow_finishes`, `raw_high_water` in the stats). Nothing is dropped: the
  whitespace after the last chunk goes out as a chunk of its own, so the byte-for-byte promise holds
  across the cut and a barge-in in the text after the flood still truncates the context to the
  played words (review fix; before it the dropped run made `apply_truncation` report `NotAPrefix`
  and the unplayed text stayed in the context). `TtsStage` records a whitespace-only chunk in the
  book as sent text but never sends it to the provider and opens no context for it; with no
  context open yet it keeps the whitespace (at most `WHITESPACE_LEAD_CAP` = 64 KiB per round,
  conservative; beyond it the whitespace is dropped and counted in `whitespace_dropped`, and that
  one degenerate round falls back to the safe `NotAPrefix`) and records it with the round's first
  spoken chunk. Tests: `a_whitespace_flood_does_not_grow_the_retained_text`,
  `the_overflow_cut_loses_nothing_the_chunks_still_concatenate_to_the_message` (segmenter stage,
  including the truncation) and the three `whitespace_*` tests in `tests/tts_stage.rs`.
- **Who records the sent slices.** The `PlayoutBook` is keyed by utterance, and the utterance
  (provider context) is minted by the TTS stage, so `TtsStage` calls `push_sent` with the raw
  slice just before the (normalized) text goes to the provider; the segmenter stage has no
  utterance to record against. The book therefore holds exactly what was sent, text dropped for a
  dead context is not recorded, and a failed send marks the utterance lost.
- **Utterances and the end of a response.** One provider context per text-bearing round, id from a
  session-monotonic counter (`ContextId` = `UtteranceId` = `UtteranceStarted.utterance` =
  `WordTimestamp.utterance`), registered as `(epoch, round)` where the round is the number of
  `LlmResponseStarted` frames of the epoch so far, minus one (the ordinal the runner registers).
  `TtsEnded` is emitted exactly once per response, decided in one mutex-guarded ledger shared by the
  stage and its reader task: when `LlmResponseEnded` arrived and no context of the epoch is
  pending. Ordering rules that make that safe: the provider `Flush` goes out BEFORE the
  `LlmResponseEnded` frame is forwarded (the last words wait for it); the ledger is updated AFTER
  the audio tail of a flushed context was handed on (a `TtsEnded` can never overtake audio);
  whoever finds the condition true last emits it. A response without text, with a dead provider
  or whose contexts were lost ends at once; a duplicate `LlmResponseEnded` is counted and ignored.
- **The reader task owns the `OutputFramer`.** Provider PCM at `EngineRates::tts` is converted to
  the call's output rate (`CallRatesCell`, published by the pump through `Pump::spawn_publishing`;
  the engine's fallback rates until `Started`) and cut into exact `EngineRates::frame` frames; the
  last frame of an utterance may be shorter (padding would put the playback position ahead of the
  words). The resampler's ramp-in is dropped, so word offsets stay valid on the output timeline
  and a whole utterance has exactly the input's duration. Per provider chunk: two allocations
  (block + shared counter) when the previous chunk's frames are still alive, none otherwise; none
  per frame (`tests/tts_alloc.rs`, counting allocator; same for 20 ms and 1 s chunks). Per word
  frame: one `Box`. A chunk is converted in pieces of at most `max_piece` (100 ms of provider audio,
  about 30 us for the worst pair, 24 kHz to 8 kHz) with a yield in between, so a long provider chunk
  never holds a worker; the resampler (0.6-1.6 ms to build for 8 kHz output) is built on the
  `Offload` at the first audio of a call. `UtteranceStarted` is emitted before the first audio of an
  utterance and again whenever the audio switches between contexts (the framer tail of the
  previous context is flushed first, or dropped if that context was cancelled). Every send is
  abandoned when an interrupt for a newer epoch is queued for the stage (words excepted, they
  are exempt from the epoch rule). A SEND TO THE PROVIDER abandoned that way leaves its context in
  the ledger (so the interrupt cancels it at the provider) and marks the utterance lost in the
  book (its text did not all arrive); only a timed-out or refused send loses the context and ends
  the response without it. Tests: `tests/tts_faults.rs` (full input channel: timeout, lost
  context and the end of the response; interrupt vs a waiting send with a 30 s timeout; full
  cancel channel; a wedged consumer behind the reader).
- **Interrupt.** The stage drops the ledger entries of older epochs and `try_send`s a cancel per
  open context on the provider's priority channel; events of unknown contexts are dropped; the
  reader resets its framer when it finds the context it was framing gone. A NEW epoch that starts
  without an `Interrupt` (the runner dropped a turn) cancels what the old epoch left open the
  same way. Test: `an_interrupt_cancels_the_context_and_late_audio_never_comes_out`.
- **Provider faults** (tests with `MockTts`): the session connects at `Start` (prewarm) and again
  when a round with text finds it dead; connect is bounded by `Timeouts::connect` on the clock and
  abandoned on an interrupt; after a failure the stage backs off `reconnect_backoff` counted from
  the moment the session DIED (the reader stamps it, so a session that died turns ago is retried
  at once), drops the text of the round meanwhile (counted, the response still ends), and gives up
  until the next turn after more than `max_reconnects` consecutive failures; a session that
  delivered audio or lived 10 s is healthy and does not count. A session that dies mid-utterance
  marks its utterances lost in the book (`mark_lost`, so the heard prefix stops before the first
  untimed word and the message is never kept whole) and emits the owed `TtsEnded`; the rest of the
  round whose context died is NOT resurrected on the new session (its words would restart at
  offset zero; test `the_rest_of_a_round_whose_session_died_is_dropped_not_replayed_on_a_new_session`).
  A provider that is silent for `idle_timeout` (default `Timeouts::llm_idle`, 15 s) while it OWES
  events is declared dead the same way (`stalls`). It owes events from the first text it has not
  reacted to (any audio or word of the context answers it) or from a `Flush` until its `Flushed`;
  an open context whose input was all answered owes nothing, so a round that streams slowly (a
  long tool-call argument between two sentences of one round) is not a stall (review fix: the timer
  used to run whenever a context was open). The clock starts at the first unanswered input, not at
  every new chunk, so a silent provider is still found while text keeps coming. Tests:
  `a_round_that_streams_slowly_is_not_a_stalled_provider`,
  `silence_after_late_text_is_a_stall_counted_from_that_text`,
  `an_unconfirmed_flush_is_a_stall_even_after_all_text_was_answered`. `Stall` faults that end by
  themselves just delay the response.
- **`TransportOut` is not a pacer** (D8). It hands each frame to the session's command
  queue as it arrives; the bounded queue pushes back on the TTS side, the session's own
  `OutputPacer` spaces the frames 20 ms apart (test: 25 frames accepted by the stage within
  10 ms reach the client exactly 20 ms apart). A send is polled once without a timer, then waits at
  most `Timeouts::send` and not past the end of the epoch; a frame the transport does not take
  marks its utterance lost. The stage is `KeepAll` and filters epochs itself so it can count
  (`stale_dropped`, `stale_control`): the newest of {its own `Clear` epoch, the last `Interrupt`}
  is the floor. **Epoch ledger rule:** `Clear { epoch }` before the first audio of any epoch above
  the stage's `tx_epoch` (no `Interrupt` is needed, the second turn of a call is the case that
  matters), idempotent with the turn driver's own clear on a barge-in; an `Interrupt` marks
  `AudioOutStopped` on the epoch whose audio was playing and sends an idempotent `Clear`. The
  first audio the transport accepted marks `FirstAudioOut` and sends `TtsStarted` upstream, once
  per epoch.
- **End mark.** `TtsEnded` becomes `Mark { id: "brisvox-end-<epoch>-<n>" }` (unique per session),
  registered in the book for the response's LAST utterance before it is sent (the echo can come
  back at once). The echo reaches the pump (`MarkPlayed`), which sends `TtsEnded` back through this
  stage; it is let through once per epoch (`ended_through`), so the echo, a `mark_timeout` end and a
  response end without audio can never report the same epoch twice. A client that never echoes: the
  stage ends the response `Timeouts::mark` (10 s) after the audio should have played out (first
  handoff plus the audio handed over), one `DeadlineTimer` woken through `SystemFrame::Wake`;
  a late echo is a counted duplicate. A cut epoch never reports an end: its stale `TtsEnded`,
  `UtteranceStarted` and late mark echo are dropped and counted (`stale_control`), its pending
  mark is forgotten at the `Interrupt`, and the overdue mark of an epoch the transport already
  moved past ends nothing (tests `a_cut_epoch_never_reports_an_end_however_its_end_signals_arrive`,
  `the_mark_of_an_epoch_the_transport_moved_past_times_out_silently`; the timeout test checks the
  deadline at 3.1 s and 3.3 s against a 3.2 s deadline, so a deadline taken from the arrival of
  `TtsEnded` fails it). `FirstAudioOut` and `TtsStarted` describe the moment the transport took
  the frame, not its arrival (test `the_first_audio_out_waits_until_the_transport_has_taken_the_frame`
  blocks the first send for a second), and an `Interrupt` alone clears the client
  (`an_interrupt_alone_clears_the_client`). A response that sent no audio is ended at once, without a
  mark. Known gap, deliberate: only the last utterance of a response has an end mark, because an
  echo of an earlier one would end the response early (the pump cannot tell them apart); the
  truncation reads the earlier utterances from positions.
- **Rates and mu-law.** The engine delivers PCM16 at `CallRates.output`: 8 kHz for a
  telephony leg (24 kHz to 8 kHz uses the sinc backend). The mu-law/A-law encoding is the
  TRANSPORTS' job (`brisvox_audio::Codec` inside the Twilio, Telnyx, SIP and websocket adapters,
  which advertise 8 kHz as the leg's rate); the engine never sees G.711 bytes. What is left to check is the
  8 kHz mu-law WebSocket end-to-end test with a 24 kHz TTS.
- **Seams added for this stage.** `CallRatesCell` + `Pump::spawn_publishing` (the TTS stage is
  built before the pump reads `Started`); `brisvox_sim::InputTarget::bind_upstream` (default no-op,
  called by `SimSession::run_with` right after `bind`: a target that owns a pump needs the started
  pipeline's UPSTREAM input for the mark echoes; the reference factory is unaffected);
  `brisvox_engine::Error::Audio`. The sim edit is additive (a trait method with a default no-op
  and an `InputSink` method called once by the runner, `session.rs`); `brisvox-sim` still does not
  depend on the engine. Test: `crates/brisvox-sim/tests/seam.rs`, the whole-suite test counts one
  `bind_upstream` per session, after `bind`, and `a_target_gets_the_upstream_input_and_other_sinks_ignore_it`.
- **Cost of `Segmenter::push` (50 us rule).** Release median 0.2-1.2 us, p99 up to 2.8 us, worst
  push up to 109 us (a 12,000-char token without whitespace), for sentences, long clauses without
  punctuation, a whitespace flood and one long token. Far inside the 50 us rule, no pool needed.
  The numbers and the asserted bounds (median, p99 and worst, each the best of three runs) are in
  `docs/LATENCY.md`, "Segmenter push cost in the stage"; the test is `tests/segmenter_stage.rs`,
  `a_push_costs_less_than_the_worker_budget`. The gate runs it in debug, where its bounds are loose,
  so the machine-independent guard is `tests/segmenter_alloc.rs` (a counting allocator: allocations
  per chunk, and at most 3 per push for text with sentence ends; a per-character allocation fails
  it).
- **A silent turn tells the driver twice.** For a turn without text the `LlmRunner` sends its own
  upstream `TtsEnded` (nothing downstream would report the end), and the `TtsStage` still ends the
  response, which `TransportOut` turns into a direct end (no audio, no mark). The runner's frame
  travels from the runner up to the driver and never passes `TransportOut` (which is downstream of
  it), so no stage can deduplicate the two: the driver receives two `TtsEnded` for a silent turn
  and one for a spoken turn (the mark echo). Harmless: the turn machine ignores `BotAudioEnded`
  while idle. "Exactly one `TtsEnded` per response" therefore holds per stage (`TtsStage`,
  `TransportOut`), not for the whole chain. Test:
  `hybrid_output.rs`, `a_silent_turn_ends_for_the_driver_and_the_next_turn_still_speaks` (counts
  3 for a silent turn plus a spoken one, and that the second turn is spoken).
- **Tests.** `tests/segment_spans.rs` (agent), `tests/segmenter_stage.rs`, `tests/tts_stage.rs`
  (20: frames and words, rounds, empty response, ids, marks, resampling to 8/16/24/48 kHz, interrupt,
  normalization inline and offloaded, no word timestamps, disconnect before and after the end,
  stall that completes, silent provider, failed connect and backoff, connect timeout, degraded
  budget, upstream, dropped turn, and a real-time run with the normalization and the 8 kHz sinc
  resampler on a real inference pool), `tests/tts_faults.rs` (4: provider and downstream that stop
  taking things), `tests/tts_alloc.rs` (the hand-built model path AND the real stage under a counting
  allocator, 20 x 20 ms vs 20 x 1 s chunks must cost the same), `tests/transport_out.rs` (12: two turns
  without a barge-in, stale audio after a barge-in, only the session paces, first handoff, unique
  marks and their echo, a client that never acks, a response without audio, words and positions in
  the book, unattributed audio, a burst larger than the queues, a transport that takes nothing, an
  interrupt that abandons a waiting command send),
  and `tests/hybrid_output.rs` (gate 4).
- **Gate 4** (`tests/hybrid_output.rs`): engine pump, `AudioIn`, VAD, `SttStage`, `TurnDriver`,
  `LlmRunner`, then `SegmenterStage`, `TtsStage`, a recording tap and `TransportOut` on a real
  `TransportSession` over the loopback transport, judged by the reference harness. s01..s11 under
  Budget with fixed silence (recommended guards and none), the adaptive policy with a fixed mock
  model, every scenario but s06 with the default retry, a registered tool, the basic suite, the
  fixtures; plus wire-level checks (nothing of the cut epoch after the client's clear; both rounds
  of a tool turn spoken as two utterances with one end mark; counters). The loopback client's
  events carry their arrival times and are recorded after the run (a client queue large enough for
  the whole call), giving the suite's wire records and its playout log. The cut context is read
  from the `PlayoutBook` the stages write, through a minimal truncation source in the test
  (`BookSource`: settle wait for interrupted responses, `RoundSnapshot::heard`, no lead margin);
  it was later replaced by the production `PlayoutTruncation`. **Not run on this chain:** the played-text suite
  (`suite::played_text`): its oracle reads the frames at the tap, which sits BEFORE the transport's
  pacer here, so it disagrees with the playout (wire) log by seconds; the wire-arrival
  oracle (`LoopbackClient` `received_at`, +-1 frame) replaces it.
- **Not covered here.** (The production truncation source and the wire-arrival oracle are in the
  played-text section, below.) Assembly (`run_call`, queue wiring, prewarm test); chaos, 100 concurrent calls, the
  real-time multi-thread run and the micro-benches of the resampler under load; live
  providers (Cartesia's `continue`/`flush` semantics, word timestamps of ElevenLabs/Cartesia
  against the normalized text).

  real `TtsStage` consuming the ordinals.


### Played-text truncation from the PlayoutBook

- **The source.** `PlayoutTruncation` (`truncation.rs`, `PlayoutTruncationConfig::from_engine`:
  `lead_margin`, `settle_wait`, `PlayedConfig`) is the production `TruncationSource`;
  `LlmRunnerBuilder::with_playout(book)` is `with_book` plus it. The runner is unchanged: it
  already asked last round first at `UserTurnEnded` and `Stop`, bounded by `analysis_job`, and
  asserted `Context::validate` after every cut. The source answers `Keep` for a round
  the book no longer knows or with blank text, `HeardInFull` for a round played to its end, and
  `Heard` otherwise (`PlayedText::heard` at the heard position, an empty prefix for a round never
  reached).
- **Settle wait.** Only for an interrupted response. `wait_settled_through(epoch)` on the injected
  clock for what is left of `settle_wait` after the barge-in (`interrupted_at`), not after the
  question: the transport flushes the final position within moments of the clear, so a position
  still missing 250 ms after it is not coming, and the caller usually talks for longer than that
  before the turn ends. A timeout is counted (`settle_timeouts`) and the last known position is
  used. After a timeout the rest of the pass does not wait again (a short memo), so N rounds
  never cost N waits. `settle_wait` must stay well below `Timeouts::analysis_job` (the runner's
  bound for the whole pass): `EngineConfig::validate` requires `settle_wait <= analysis_job / 2`
  and `PlayoutTruncationConfig::from_engine` clamps to it, so the pass never hits the runner's
  deadline (which would answer `Keep` and leave unheard text in the context).
- **What the wait ignores.** Utterances of rounds already cut (`mark_applied`), and cut
  utterances of rounds without a context message (filler and fallback rounds, `msg_index` none):
  nothing is ever cut to their position and the runner never applies them. Before this rule a
  filler cut between two position reports (no final position, no end mark, not all released)
  stayed unsettled forever, so every later interrupted pass waited out `settle_wait` and the
  runner's speculation answered `skipped_busy` for the rest of the call. Not waiting for it costs
  nothing that matters: its stale, lower `released` only makes the chain of an earlier utterance
  more cautious. A round that gets its message later (a partial message at the barge-in) is
  waited for again from then on.
- **Timeouts are expected.** The transport flushes a final position only if audio was released
  since its last report, so a clear right after a report has none to send. With a report per
  20 ms frame (the tests) that is every barge-in between two frames, with the 100 ms production
  interval about one in five mid-utterance barge-ins. The last report IS the final position then;
  the counter is not a failure signal by itself. A transport that always flushes (or acknowledges
  the clear) would remove the case and close the stale-report limit of the `playout` module docs.
- **Heard position.** `min(released, total) − lead` (see section 5). A response that ended
  without an interruption loses no lead (it has played out by the time the next turn starts), so
  a client that never echoes marks does not lose its last words. That includes the client's last
  reported lead (Twilio): an earlier moment's buffer level is not subtracted from a response
  that has played out.
- **The margin fades after the last release.** The fixed `lead_margin` assumes audio was still
  going out at the barge-in. A tool round's utterance has no end mark (one mark per response), so
  a caller barging in during a 5 s tool call would otherwise lose the last word of "Let me check
  it" although the buffer drained seconds ago. The book stamps, on the call's clock
  (`PlayoutBook::attach_clock`, done by `PlayoutTruncation::new`), the time a position report
  last moved `released` for each utterance; `RoundSnapshot::margin_at` takes off the margin the
  time between the latest stamp of the utterance or of the utterances behind it and the barge-in.
  It is a lower bound of playback as long as the client's buffer never exceeds the margin: what
  the client held at the stamp (at most the margin) plays out in real time. A stamp is taken when
  the report arrives, at or after the real release, so the fade is never too large. A reported
  client lead (Twilio) is measured and is not faded. Tests: `playout::tests` (stamps and fade),
  `the_margin_shrinks_by_the_time_since_the_last_release_and_is_gone_after_a_tool_wait`,
  `a_barge_in_during_tool_execution_closes_the_call_and_keeps_what_was_spoken` (runner, 160 ms
  margin) and the wire-level `after_a_tool_wait_the_production_margin_does_not_take_the_last_word_back`.
- **Who tells the book about the clear.** `playout::clear_hook(commands, book)` is the
  `TurnDriver` clear hook: `book.note_clear(new_epoch)`, then `TransportCommand::Clear`. The note
  is made when the hook is called, before its future is first polled, so it precedes the command
  and the final position the transport flushes right after it (pinned by
  `the_clear_hook_tells_the_book_before_it_sends_the_command` and, over the loopback transport,
  `the_clear_hook_tells_the_book_before_the_clear_reaches_the_client`). Use it
  instead of `TurnDriver::with_commands` wherever the production source reads the book. The
  assembly MUST install `clear_hook` on the TurnDriver and `LlmRunnerBuilder::with_playout` on the
  runner with the SAME book the stages write. Without the note a
  mid-utterance cut never settles (every barge-in waits out `settle_wait`) and the client's lead
  is not tied to its clear. Miswiring is detected, not silent: `BookStats::clears_noted` counts
  the notes, and `PlayoutTruncation::unnoted_clear_cuts()` counts interrupted responses asked
  about while it was zero (first occurrence logged as an error on `brisvox::engine`); the
  assembly's tests assert it is zero.
- **A mark that is echoed after the clear.** A race in `TransportOut::tts_ended` (found by the
  wire-oracle test): the `TtsEnded` of the cut response can be handled after the transport applied
  the `Clear` but before the `Interrupt` reached the stage's epoch watch. `CommandSender::send`
  reads the ledger epoch when the command is enqueued, so the `Mark` counts as the NEW epoch's,
  is released at once (nothing is ahead of it) and echoed by the client. The book treats an echo
  that arrives after a clear covered the utterance as a cut mark, so the position decides; the
  stale `Mark` on the wire (and the `BotAudioEnded` it makes upstream) remain. A proper fix is in
  `TransportOut` (check the book's clear floor before sending a mark) or in the transport
  (`TransportCommand::Mark` carrying its epoch); neither is part of the play-out book itself. The same rule has a cost: a GENUINE echo of a fully played response that is still
  in flight when the caller barges in is read as a cut mark too (the book cannot tell them
  apart), so the cut falls back to `min(released, total) - lead` and can drop up to one lead of
  the end of a sentence the caller heard (test
  `a_genuine_echo_that_races_a_barge_in_is_cut_by_position_not_kept_whole`).
- **Followed by a played round.** A round counts as played whole when a LATER round of the SAME
  epoch was echoed as played; rounds of other epochs never vouch for it (a clear separates
  epochs, so a round cut by a clear stays a position-based cut even after the next turn's answer
  was acked).
- **Wire-level margin and latency.** The 40-case oracle runs twice: with `lead_margin` 0 (wire
  arrival) and with the production 160 ms (the context is the wire position less 160 ms), and
  both assert that the next turn's LLM request leaves within one frame of the turn end (a settle
  wait measured from the question instead of the barge-in would add 250 ms and fail), and that
  at least one interrupted call settled on its flushed final position without a timeout (a book
  never told about the clear times out every one). The second-round tool test also runs with
  the 160 ms margin and a first round whose last word starts inside it: only the chain over the
  second round's audio keeps that word (`with_the_production_margin_the_first_round_is_chained_behind_the_second`).
- **Tests.** `tests/playout_truncation.rs`: the book driven by hand on the paused clock (exact
  margin and lead numbers, the lead chained over later utterances, the margin fade, the settle
  wait and its memo, a missing final position, a cut filler that never settles, the clear hook's
  ordering, kept-whole rules, the runner's partial messages, a tool round cut in round two whose
  first round's last word only the chain keeps, a barge-in during tool execution, nothing heard
  of a message with a call).
  `tests/playout_oracle.rs`: the whole production pipeline over the loopback transport
  (`tests/common/output_rig.rs`, shared with `hybrid_output.rs`), judged against WIRE arrival:
  40 seeded barge-ins with a ±1 frame window, a proof that the window does not hide a deliberate
  one-word error (a source wrapper that drops or adds a word: every case with a single-count
  window is caught), a word starting in the unplayed rest of the chunk in flight, no word
  timestamps (the 14 chars/s estimate of the wire position), a TTS disconnect (the exact spoken
  prefix), a tool turn with a barge-in in round two and during execution, a barge-in before any
  audio, and the filler round (a played filler is put into the context and cut to what
  was heard like any round: `the_filler_round_is_cut_like_any_round_and_kept_as_heard`). The shared
  `suite::played_text` checks that read the output TAP cannot run on this chain (the tap precedes
  the pacer). The seven checks map as follows. Run as they are: `a_fully_played_answer_is_kept_whole`,
  `timed_barge_ins_are_counted_and_never_estimated`,
  `a_fully_played_answer_without_word_timestamps_is_not_estimated_away`. Wire-oracle twins:
  `random_barge_ins_leave_the_context_at_exactly_the_heard_text` (the 40 seeded barge-ins),
  `a_word_starting_in_the_unplayed_part_of_the_chunk_in_flight_is_not_heard` (same name),
  `a_barge_in_is_counted_and_a_provider_without_timestamps_is_flagged_as_estimated`
  (`without_word_timestamps_the_cut_is_the_speaking_rate_estimate_of_the_wire_position`, which
  asserts `TruncationStats::estimated`), and
  `a_barge_in_while_the_llm_is_still_streaming_keeps_only_the_heard_part_of_what_was_sent` (same
  name: the LLM stalls before its third delta, the cut is judged against the delivered text on
  the wire and the aborted tail never reaches the context). The `EngineFactory` tap records everything but audio as it passes; audio is recorded at wire
  arrival, so the shared played-text suite runs unchanged on `EngineFactory`. See "Assembly" below.

### Speculative generation

Code: `crates/brisvox-engine/src/{speculation,llm_spec}.rs` plus additive parts of `turn_driver.rs`,
`llm_runner.rs`, `llm_turn.rs` and `config.rs`. Opt-in: `EngineConfig::speculation`
(`SpeculationConfig`, default `OFF`: every pipeline behaves exactly as before).

- **Idea.** At a candidate end of turn the LLM is started with the transcript so far; its output is
  *buffered*. When the turn really ends with the same words the buffer is released at once, so
  the model's time to first token is paid while the machine is still waiting for the end of the
  turn (silence timer, Smart Turn, finals gate). Otherwise the run is dropped and the turn runs as
  without speculation. The conversation, the frames and the words on the wire are the same either
  way; only the latency differs.
- **Protocol (frames only).** The driver never forwards interims, so a non-final `Transcript`
  downstream of the driver is the speculation signal: non-empty text is a *candidate* for the open
  turn (a later one of the same epoch replaces the earlier), empty text is a *withdrawal*. They
  travel in the same lane as the final `Transcript` and `UserTurnEnded`, so there is no race
  between a candidate and the commit. `UserTurnStarted`, `Interrupt`, `Stop` and `Cancel` also
  drop a speculation. (A dedicated `ControlKind` would be cleaner; it needs a change in
  `brisvox-core` and was left out to keep the change additive.)
- **Candidates (TurnDriver).** (1) At the `UtteranceFinal` of a stop, if the machine is paused in
  an open turn and no other stop of the turn is owed: the turn's final text. (2) With
  `on_partial`, at the VAD stop that paused the machine: the final segments heard so far, else the
  latest interim; with `partial_stable_for` > 0 (the stable-partial gate) that partial
  waits until the caller stayed silent and the text unchanged for that long (a resume or the
  stop's own final cancels the wait, so a caller who keeps talking costs no request; a final is
  the STT's own stable text and is never held). Equal (normalised) text is not offered twice; `max_per_turn` (3) bounds the
  requests a stuttering transcript can cost. A resume (`VadStart`), or a gate release with no text,
  withdraws the candidate. With the adaptive policy a complete verdict ends the turn at the final
  itself, so (1) adds nothing there and costs nothing.
- **Hit rule (LlmRunner).** At `UserTurnEnded` the speculation is committed when it belongs to the
  same epoch, `normalize(final) == normalize(candidate)` (lower case, letters and digits only,
  apostrophes dropped, so punctuation and spacing differences do not count), it is younger than
  `max_age` (5 s), and the context is exactly the one its request was built from (a late tool
  note that arrived meanwhile is a miss).
  - *Normalisation.* `İ` is `i` (a Turkish capital must equal the interim's lower case), and a
    combining mark belongs to its letter: the composed (NFC) and decomposed (NFD) spelling of a
    word are equal for the Latin letters of a small compose table (Turkish `ç ş ğ ö ü â î û`, the
    common French, Spanish and German accents). A mark with no table entry is kept as it is
    (it can only miss, never produce a wrong hit); accents are never folded (`şu` is not `su`).
    Symbols that change what a reply or a tool argument would be are kept, so they never compare
    equal to their absence: every currency sign (the Unicode currency block, `$ £ ¥ ¢ ¤`, the
    Turkish `₺`), `% ‰ @ + = < > / & # * ~ ^`, `° ± × ÷`, primes, and the joints inside a number
    or an address (`-50`, `3.5`, `10:30`, `john.doe`).
  - *Context identity.* The fingerprint of the context (length, and per message the role, the
    byte length and the last 16 bytes of the text, the tool call ids) is taken at the settle and
    compared at the turn end. It is O(messages), not O(bytes): it runs on a tokio worker twice
    per speculated turn, once under the context lock, and a context can hold hundreds of KB of
    tool results. A settled context only ever grows or has a message cut to a prefix, which the
    length and the tail see.
- **Settle.** A candidate settles the context exactly as the turn end would, minus the user
  message (`halt`, played-text truncation, open calls closed, late notes in). The truncation wait
  is therefore hidden by the speculation too, and the turn end finds nothing left to settle. A
  runner that is still answering the previous turn (`skipped_busy`), drives a flow
  (`skipped_flow`: a pending transition would change the context behind the request) or cannot
  settle (`skipped_unsafe`) ignores the candidate.
- **The helper (`llm_spec`).** One `TaskGuard` task per speculation. It builds the request from a
  *copy* of the context plus the candidate (the real context is untouched until the commit), runs
  the first round with the round task's first-token and idle timeouts, and sends the pieces
  `LlmText` would carry into a bounded channel (`max_buffered_events`, 256, validated to at most
  `SpeculationConfig::MAX_BUFFERED_EVENTS_LIMIT` = 4096 so the bound is real, and clamped to it
  again where the channel is made; a full buffer back-pressures the provider stream, nothing is
  dropped). It ends by itself at `max_age` unless
  the turn claimed it, and when its token is cancelled. Nothing reaches the pipeline, the
  `PlayoutBook`, the context or the timeline from it.
- **Commit.** The runner gives the receiver, the helper's guard and its token to the round task.
  Round 0 of that task replays the buffered pieces at once (`LlmResponseStarted` first, so the
  `PlayoutBook` registration and the frames of the round are those of an ordinary round) and then
  follows the live stream; the timeline gets the instants things really happened at (request
  when the speculation started, first token when it arrived). A failed speculative stream counts
  as the first attempt: before any output the round is retried with a fresh request if the retry
  budget allows, after output the partial text stays, exactly as for a normal round. Later rounds
  (after a tool phase) are ordinary rounds.
- **Tools.** At `Done` the helper finishes the assembler as the round task does and, with a tool
  engine and no flow, calls `ToolEngine::speculate`: ReadOnly calls run now, SideEffect calls are
  *held* inside the `Speculation`. At the commit the round task pushes the assistant message, sends
  `Flush`, and runs `ToolEngine::commit` (raced against the filler timer): the held calls run
  there, once, under the engine's idempotency key `(session, epoch, call id)`. A discarded
  speculation drops them unrun; a ReadOnly call in flight is cancelled with the helper. Without a
  tool engine, or with a flow, the calls are handled after the commit as always. While the helper
  runs the ReadOnly tools no idle timeout applies to the committed round (the tools have their
  own).
- **Metrics (`LlmRunner::speculation_stats()`).** `started`, `hits`, `misses` (text changed),
  `context_changes`, `expired`, `superseded`, `withdrawn`, `abandoned`, `kept`, `skipped_*`,
  `failures`, `tool_phases`, `held_side_effects`, `wasted_output_tokens` (provider-reported, else
  4 bytes per token) and `wasted_input_tokens` (provider-reported), `head_start_ms`; the snapshot's
  `hit_rate()` is hits over decided speculations. The driver counts `speculations_offered`,
  `speculations_withdrawn` and `speculations_capped`.
- **Costs and limits.** Every discarded speculation is a billed request; `on_partial` trades more
  of them for a head start of one STT round trip (real interims often lag the final by a word, the
  mock's lag by exactly one). The engine's per-turn retry budget for malformed tool calls is spent
  by a speculation even if it is discarded (`ToolEngine::speculate` documents it). A speculation
  covers the first round of a turn only. Speculation with a flow is a follow-up.
- **Default stays off.** Measured on the fixtures and over the websocket
  (`docs/LATENCY.md`, "Speculative LLM start"): under the adaptive policy the gain is one STT
  finalize round trip (about -150 ms) and only when the partial at the VAD stop equals the final;
  a lagging partial costs one extra request on every turn and gains nothing. Under a fixed silence
  longer than the finals gate, speculating at the final alone (`on_partial` off) returns the
  silence beyond the final (-255 ms at 700 ms) with no wasted request. Enable per deployment.
- **Tests.** `tests/speculation_runner.rs` (hit and buffering, identical frames and request,
  miss, withdrawal, replacement, drop on new turn / barge-in / stop, other epoch, off, SideEffect
  held then run once, discarded unrun, restarted once across two runs, ReadOnly ahead and used,
  ReadOnly cancelled, slow tool without idle timeout, late note makes a miss, expiry, retry,
  failure after output, back-pressure, barge-in after the commit, no leaked tasks, the filler
  during a committed tool phase, a SideEffect call that still runs after a miss whose
  speculation had held the same call id, the helper's first-token and idle timeouts),
  unit tests in `llm_runner.rs` (a claimed speculation keeps its helper past `max_age` while the
  turn settles; an expired one cannot be claimed) and in `llm_turn.rs` (the committed round's own
  first-token and idle limits, no idle limit in the tool phase),
  `tests/speculation_driver.rs` (when candidates and withdrawals are offered), and
  `tests/speculation_hybrid.rs` (a hybrid call with and without speculation: identical context and
  words, response latency lower by the model's time to first token minus the wait it overlapped,
  lagging interim, resumption with exact offered / withdrawn / started / hit counts, a tool turn,
  the adaptive policy with exact counters per variant, and the structural scenarios s01, s02, s04,
  s05, s07 to s11 with candidates flying and at least one committed speculation in a barge-in
  call and in a tool call).
  The adaptive-policy test documents what is measured: the mock STT's last interim lags the final
  by a word, so with `on_partial` the partial candidate at the stop is a **miss** (one wasted
  request, same context, same latency); a hit on a partial candidate is covered only at the
  runner level, because the mock never produces an interim equal to the final.

### Connection pre-warming and pooling

Goal: no TCP, TLS, DNS or WebSocket handshake on the critical path of turn 1 (or of any later turn).

- **Audit (what existed).** `SttStage` and `TtsStage` connect at `Start`; `LlmRunner` calls
  `LlmProvider::warm()` at `Start` in a `TaskGuard` under `Timeouts::connect` and counts `warms` /
  `warm_failures`; the OpenAI, Anthropic, Gemini and OpenAI-compatible adapters warm with an
  unauthenticated `HEAD`; Deepgram warms DNS, Cartesia parks one socket, ElevenLabs warms its
  socket; Deepgram sends `KeepAlive` after `keepalive_interval` without audio and ElevenLabs keeps
  a quiet context alive. TLS is built once per process (`net::tls_config`).
- **Gap 1: one HTTP client per adapter instance.** Every call built its own `reqwest::Client`, so
  every call paid a TCP + TLS (+ HTTP/2) handshake on its first request and the `warm()` HEAD warmed
  a pool nobody else could use. Now `brisvox_providers::net::shared_client(&HttpClientSpec)` hands
  out clones of ONE client per (origin, connect timeout, pool idle timeout, HTTP/2 prior
  knowledge) from a process-wide map, used by the four HTTP/SSE adapters. Credentials stay in
  per-request headers, so different keys share the connections. The map holds at most 64
  configurations (`MAX_SHARED_CLIENTS`); a new one at a full pool evicts the least recently used
  entry (`PoolStats::evicted`, one `warn` log), so sharing never switches off. `net::pool_stats()`
  exposes `built` / `reused` / `evicted` / `entries`. The clients survive the tokio runtime that
  opened their connections: the next request under another runtime reconnects (unit-tested).
  Clients keep TCP keepalive (30 s) and HTTP/2 PING keep-alive while idle (20 s / 10 s). Tested
  over a real TLS loopback server (`net/http/tls_tests.rs`, built through `build_with`, the
  function behind the production client): ALPN settles on `h2`; a `warm()` HEAD followed by two
  more requests makes ONE TLS handshake; an idle connection sends PINGs (interval shortened to
  100 ms in the test, the 20 s production value stays quiet); a system proxy setting is ignored
  (child process with proxy variables). Not observable from the peer and so not pinned: the TCP
  keepalive socket option.
- **Gap 2: STT and TTS never called `warm()`.** Both stages now warm the provider at `Start`, before
  the first connect, under the connect timeout on the injected clock (`SttStatsSnapshot` /
  `TtsStatsSnapshot` gain `warms` and `warm_failures`). A failed or timed-out warm-up is counted and
  never fails the call: the connect follows regardless. Reconnects do not warm again, tested on
  both socket stages in `hybrid_prewarm`: the first STT session dies while the call is quiet and
  the reconnect starts exactly one reconnect backoff after the death (no warm-up gap in front of
  it, one warm-up for the call); the first TTS session dies the same way and is replaced at the
  next response start, again with one warm-up for the call. The warm-up has a budget of its own, `min(connect timeout, 400 ms)`: a warm-up that stalls (a probe that a
  proxy swallows says nothing about the real connect) delays the connect by 400 ms at most, not by
  the whole 5 s connect timeout, and the TTS processor is not held for 2 x 5 s. (The LLM warm-up is
  a spawned task under `Timeouts::connect` and never holds anything.)
- **Keepalives.** HTTP/2 PING while idle (20 s / 10 s) and TCP keepalive on the shared clients;
  Deepgram `KeepAlive`; the ElevenLabs context keepalive. Cartesia documents no TTS keepalive and
  closes a socket after 5 minutes idle (and advises opening a new one when needed), so the TTS
  stage replaces a session that died while the call was quiet when the next response STARTS
  (`LlmResponseStarted` finds a dead session): the reconnect overlaps the LLM's time to first
  token instead of waiting for the first text. Proved in `hybrid_prewarm.rs` (idle death, then a
  turn: the connect starts with the response and is done before the first token). A reconnect
  longer than the LLM's first token still shows on that turn (the cost of a connect minus the
  TTFT); a proactive refresh before the idle limit would remove it and is a follow-up (the stage
  has no timer hook: it needs a spawned refresher that swaps the session).
- **Evidence.** `tests/hybrid_prewarm.rs` runs the production chain on a paused clock with probe
  providers that give every `warm()` and `connect()` a simulated handshake time and log it: every
  setup finished before the caller started to speak and before the first VAD stop of the timeline
  (`TurnEvent::UserSpeechEnd`, read back from the run), each socket connect starts within 1 ms of
  its warm-up's end, exactly one connect per socket stage over several turns, a second call in the
  same process (own sockets and warm-ups) with the same clean turn 1, failures and hangs of the
  warm-up counted (also with the default 5 s timeout), the idle-death cases above (TTS and STT),
  and a control run in which the checker catches a handshake that is too slow. The probe does not
  model the shared HTTP pool, so the second call's test claims only its own clean turn 1; the
  pooling itself is proved at the socket level in `crates/brisvox-providers/tests/{openai,anthropic,gemini,openai_compatible}/protocol.rs`
  (three adapter instances, one TCP connection at the mock server, per adapter) and over HTTP/2 in
  `tests/gemini/review.rs` (two adapter instances, 20 concurrent streams, one connection). Live
  numbers need live runs.
- **Not done / open.**
  - (Built, wired by the server) A proactive refresh of an idle TTS socket before the
    provider's idle limit: `ResilientTts` replaces a session idle for 80 % of
    `TtsProvider::idle_limit` (Cartesia: 4 of its 5 minutes); see "Resilience layer"
    below. No production call path wraps a provider yet (the server has no call assembly), so
    real calls still reconnect lazily on the next text until the server wiring.
  - (Done) The Smart Turn / Silero model `warm()` at Start: the VAD worker warms the
    model once per inference pool thread (`VadSetup::with_warm_up`, applied by
    `SessionBuilder::with_vad`) and the turn driver asks `TurnModel::warm(threads)` at `Start`;
    see "Assembly" below. `hybrid_prewarm` still covers the providers only; `run_call.rs` and
    `warm_pool.rs` cover the models.
  - HTTP/2 stream limits: a process-wide client multiplexes every call's SSE stream to one origin
    onto few connections and servers advertise `SETTINGS_MAX_CONCURRENT_STREAMS` (commonly 100),
    beyond which requests queue. Unmeasured against the real providers; see `docs/RISKS.md` R16
    (capacity follow-up: measure with N concurrent calls, shard the pool key if it limits).

### Assembly: `run_call` and `build_session_pipeline`

Code: `crates/brisvox-engine/src/session.rs` (library), `crates/brisvox-engine/src/sim.rs`
(feature `sim`: `EngineFactory`, the production pipeline as a `brisvox_sim::SessionFactory`).

- **Builder.** `SessionBuilder::new(config, clock, timeline, offload, stt, llm, tts)` validates
  the `EngineConfig`. The `Offload` is required (the pool in production, `Offload::Inline` only
  for paused-time tests), so an integrator cannot run real models on tokio workers by forgetting
  an option; `with_vad(VadSetup)` replaces the setup's own offload with the builder's. Options:
  `with_vad`, `with_smart_turn(model)`, language, voice, interruption policy, system prompt,
  tools, flow, summarizer, phrases, a truncation-source wrapper (tests), a `FrameTap` (tests), a
  `TurnProbe`, the book capacity.
- **Model warm-up once per pool.** The VAD warm-up (one job per pool thread) and the Smart Turn
  warm-up (one job asking for one session per thread) run once per pool and model for the whole
  process (`Offload::claim_warm`, keyed by the VAD model type and the Smart Turn model
  instance): the first call warms, later calls skip it, a failed or aborted warm-up gives the
  claim back. A burst of call starts therefore submits one set of warm jobs, not one per call,
  and cannot fill the bounded pool queue and make running calls' VAD fail open (test:
  `warm_pool.rs`). It was reworked after a performance audit: see "Warm-up claims"
  below.
- **Ending the call.** `Call::stop` (bounded by `timeouts.stop`; `Error::Timeout` after
  cancelling a pipeline that did not drain), `Call::cancel`, dropping the `Call`, and
  `Call::wait` returning (hang-up or a stage failure) all cancel the call's token, so the pump
  ends and drops the transport session. A stage failure cancels nothing by itself: the owner must
  await `Call::wait` (or stop, cancel, drop).
- **Tap upstream drops.** The test-only tap stage has the `sink` inbox (4 data frames), so the
  upstream lane `TransportOut` writes into is 4 deep instead of the TTS stage's 32. Upstream
  frames it cannot pass on are counted (`SessionHandles::tap_upstream_dropped`); the engine suite
  asserts zero for every call.
- **Chain and queues.** `pump -> AudioIn(+VAD worker) [input] -> SttStage [user] -> TurnDriver
  [user] -> LlmRunner [bot] -> SegmenterStage [bot] -> TtsStage [bot] -> (tap [sink]) ->
  TransportOut [sink]`, with the inbox shapes of `EngineConfig::queues` (input: 8 data frames,
  drop-oldest; user: 8, lossless; bot: 32, lossless; sink: 4, lossless; system lanes 32).
- **One `PlayoutBook`** per call, shared by the pump (marks, positions), `TtsStage`
  (utterances, sent text), `TransportOut` (words, audio), the runner (round to message index)
  and the `PlayoutTruncation` source, and the turn driver's clear hook.
- **Clear hook.** The driver clears through `playout::clear_hook(commands,
  book)`: `note_clear` on the book, then the `Clear` command. `SessionHandles::unnoted_clear_cuts`
  stays 0 when the hook is wired; `run_call.rs` and every `engine_suite` run assert it (removing
  the hook fails five tests). The stale end `Mark` that could reach the wire after the driver's
  `Clear` is fixed: `TransportOut` sends marks with `CommandSender::send_in`
  (the epoch they belong to), so a mark whose epoch a `Clear` ended between the stage's epoch
  check and the send is cut by the transport (`MarkCleared`), never written after the clear
  (`transport.rs` and `transport_out.rs` tests; the latter fails with plain `send`).
- **Warm-up at `Start`.** STT and TTS (provider `warm`, then connect) and the
  runner (`LlmProvider::warm`) as before; new: the VAD worker runs `SpeechModel::warm`
  once per offload thread (`Offload::warm_each`, every job submitted at once so each idle pool
  thread takes one; `VadSetup::with_warm_up`, bounded by `timeouts.connect`), and the turn
  driver runs `TurnModel::warm(offload.threads())` as one pool job at `Start` (the ORT backend
  prepares one session per concurrent analysis). Failures are counted (`warm_failures`) and never
  fail the call. A bare `VadSetup` does not warm (its factory may be scripted in worker tests).
- **Early analysis.** `SessionBuilder::with_vad` applies
  `VadSetup::with_early_analysis(&config)`, the VAD half of `EngineConfig::early_analysis`; the
  driver half is its configuration.
- **`run_call(session, builder, &cancel) -> Call`.** Builds the pipeline over
  `session.commands()`, starts it under a child of `cancel`, then starts the pump under the same
  child with the running pipeline's two inputs. `Call::wait` (caller hang-up), `Call::stop`
  (graceful, bounded by `timeouts.stop`, then ends the pump), `Call::cancel` (pipeline and pump at
  once), `Call::stats` (queue depths, drops, pump counters), `Call::handles` (every stage's
  counters, the book, the context). Dropping the call aborts every task (`TaskCounter` test).
- **`build_session_pipeline`** returns the unstarted pipeline, the handles and a `PumpWiring`,
  for harnesses that start the pipeline themselves (the simulator binds the pump in
  `InputTarget::bind_upstream`).
- **Tap.** A `FrameTap` is a pass-through `KeepAll` stage in front of `TransportOut` (it sees what
  `TransportOut` sees; `TransportOut` filters and counts stale frames itself). Production installs
  none.
- **`EngineFactory`.** Loopback transport (latency 0, marks auto-acked at once, a position report
  per 20 ms frame), real `TransportSession` and pump, `Offload::Inline`, the simulator's energy
  VAD as an `EnergyModel` behind the production VAD worker, `FixedTurnModel` for the adaptive
  policy, optional `time_in` tool. The tap records `Interrupt` and every non-audio frame as it
  passes; audio is recorded when the loopback client receives it (wire time), which is what the
  shared suite's played-text oracle needs. Divergences from the reference: `docs/TESTING.md`.

### Verification
Code: `crates/brisvox-engine/tests/{engine_chaos,realtime_load,hot_path_cost,stt_turn_alloc,
request_alloc,warm_pool}.rs`; numbers in `docs/TESTING.md` and `docs/LATENCY.md`.

- **Warm-up claims.** `Offload::claim_warm` keys: `vad:<VadSetup::with_model_id>`, else
  `vad:<SpeechModel::warm_key>` for a setup made with `VadSetup::from_model` (the ORT Silero:
  `silero-ort:<path>`), else the model type (the warm job warns when the model names a key of
  its own, so a type-keyed Silero setup is visible in the logs) and `smart_turn:<TurnModel::warm_key>` (the ORT backend: path plus a per-instance number;
  else the instance's address). `Offload::warm(claim, timeout, jobs, job)` hands a share of the
  claim to every job; the claim settles when the last share is gone: kept if every job
  succeeded, back after `WARM_COOLDOWN` (30 s, on the pool's clock) if the pool was busy (a job
  refused as saturated, timed out, or the warm-up skipped), back at once otherwise. A job still
  running after its caller's timeout keeps the claim, so a model is never warmed twice at once.
  Nothing is submitted while the pool queue holds more than `WARM_MAX_QUEUE_PER_THREAD` (1) job
  per thread (`warm_skips`): live VAD windows go first. Smart Turn warms with
  `smart_turn_warm_jobs(threads)` = half the threads (at least one) jobs at once, each asking
  `warm(threads)`; the ORT backend reserves every session before creating it, so the jobs create
  `threads` sessions in all (about two each). Spans `vad_warm` / `smart_turn_warm` on
  `brisvox::engine`.
- **VAD stream build on the offload.** The worker's own streams (first batch, after a lost job)
  are built inside the batch job, so `SileroVad::load`'s `fs::metadata` never runs on a worker.
- **Request builds.** A pool that refuses or times out a heavy build (or a heavy context copy)
  falls back to `spawn_blocking`, never to the worker (the timed-out job may hold the context
  lock); `is_heavy` also counts the whole context (`INLINE_MAX_CONTEXT_BYTES`, 256 KiB).
- **Margin fade.** `note_clear` stamps the clear; the lead margin fades only up to
  `min(interrupted_at, clear)`.
- **Stats.** `Call::stats()` returns `CallStats { pipeline, pump, stages: StageCounters }` (every
  stage's counters, the book's, the truncation counters); `CallStats::trace` writes one `call
  stats` event on `brisvox::engine` (max queued, drops, stale drops, VAD saturations and
  timeouts, pool trouble, truncations, finals-gate, settle and mark timeouts).
- **Chaos on the production pipeline.** `engine_chaos.rs` runs the shared 100-call chaos suite on
  `EngineFactory` (Budget). The slow consumer is a slow reader of the loopback client
  (`LoopbackClient::split`, a one-event client queue, `transport_delay` after each audio frame,
  frames stamped when read). Accommodations: adaptive end of turn with a fixed model, first TTS
  chunk after one word (`SessionBuilder::with_first_chunk_words`), a factory that keeps no
  handles (heap check). The first-chunk finding led to the latency tuning described in `docs/LATENCY.md`.

### Resilience layer
Code: `crates/brisvox-providers/src/resilience/` (module docs hold the full contract); tests
`crates/brisvox-providers/tests/resilience.rs` (mocks, paused clock) and
`crates/brisvox-engine/tests/engine_failover.rs` (the production pipeline on `EngineFactory`).

- **Shape.** `ResilientStt` / `ResilientLlm` / `ResilientTts` implement the provider traits over a
  primary `Leg` and an optional secondary `Leg` (a provider instance plus its `CircuitBreaker`).
  The engine sees an ordinary provider: no engine code changed. Breakers and the process
  `RetryBudget` are process-wide (`Resilience`, `Arc`s shared by every call); each wrapper
  instance is one call (its session budget, its counters, `probe()` to read them after the
  wrapper was handed to the engine). Not wrapping a stage means no retries, breaker or failover for it.
- **Operations and cost.** A connect, a request or a mid-call recovery tries the legs in order,
  each at most once (primary first; a recovery tries the leg that just failed last). A leg whose
  breaker refuses is skipped for free. The first attempt is free; each further attempt takes a
  token from the call's bucket (3, one per 20 s) AND the process bucket (50, one per 100 ms).
  Attempts followed by another run under `attempt_timeout` (2 s, inside the engine's 5 s connect
  timeout); the last attempt of a connect/request relies on the engine's deadline, every recovery
  attempt has the attempt timeout.
- **Breaker.** Consecutive provider failures across all calls (a failed connect/request, a
  session that dies before it proved healthy: first event or 10 s alive) open it after 5 for
  30 s; then one probe is let through (half-open) and every other attempt is refused. Once the
  probe connected (STT, TTS) or its response produced a first event (LLM) the breaker closes *on
  probation* (`Permit::probation`): the failure count is set one short of the threshold, so
  other calls are admitted at once and the next failure (the probe's own early death included)
  re-opens it at once; the probe proving healthy resets the count. Without probation a deployment
  with no secondary refused every other call's connect or request for up to 10 s (or a whole LLM
  response) after the provider was already back. A probe that fails before it connected re-opens
  the breaker. HTTP 400/404/413/422 and cancellations are request errors: no count, no failover.
- **Carry-over.** STT: the audio since the last final segment plus the 1 s before it
  (`stt_replay_tail`; a rolling window of at most `stt_replay`, 4 s) is replayed into the new
  session, followed by an unanswered `Finalize`. The tail exists because providers send a final
  segment hundreds of ms after the audio it covers while the audio streams on: clearing the
  window at the final lost the words of that lag on a recovery. A word at the edge may now be
  transcribed twice; none is lost to a lag up to 1 s. A `speech_final` segment (the provider saw
  the speech end) empties the window.
  LLM: a request that fails before any content (error, cut, or no event within 4 s when a
  secondary exists) is re-issued to the secondary; after content nothing is retried. TTS:
  utterances with no audio and no word timing yet are re-spoken (text and flush); a partly heard
  utterance makes the session fail as before (the engine marks it lost; resuming exactly would
  need word timings that may lag the audio). The relay reads cancels before input, so text the
  engine queued before a barge-in's cancel can be read after it: the relay remembers the last 64
  cancelled ids (as the adapters do) and drops their late text and flushes, so a cancelled
  utterance never re-opens (it would block the idle refresh for the rest of the call and be
  re-spoken by the next recovery after the engine was told `Cancelled`).
- **Idle refresh.** A `ResilientTts` session with nothing in flight for 80 %
  of the provider's `idle_limit()` (Cartesia declares 5 minutes) connects a new session in the
  background and swaps it in while still idle (a refresh that completes during an utterance
  waits as a standby until the utterance ends); the old session is dropped. Nothing is in flight
  at the swap, so nothing is heard; the next turn never pays a connect.
- **Costs.** One relay task per wrapped STT/TTS session and per LLM response; one extra channel
  hop per audio frame (STT in, TTS out); STT replay holds `Bytes` clones of up to 4 s of input
  (about 128 KiB per call at 16 kHz); TTS keeps the text of open utterances (4 KiB and 8
  utterances per session at most). Every relay ends when the engine drops its handles.
- **Proof (paused clock).** `engine_failover.rs`: all primaries down, four sequential calls:
  the secondaries answer every turn, the breakers open after two failures and the third call
  sends no request to the primaries and makes no retry; the primaries come back, the breakers
  half-open after 60 s, the probes succeed, everything closes and is served by the primaries.
  Mid-call deaths of all three stages in turn 1 (STT cut after its first interim, LLM 503, TTS
  cut before its first audio) leave the conversation identical to the fault-free one, and the
  same call without the layer does not. Both legs down with a budget of one retry: outcome Ok,
  no audio, no LLM request, at most two contacts per primary and one per secondary, no task
  left. A 3 s pause with a 2 s refresh: a refreshed TTS session, no connect on the next turn,
  first-audio times identical to the run without refresh. A barge-in through the layer: one
  interrupt, no stale audio, the cut story in the context, a refresh in the later pause and no
  connect but refreshes. Blackholed primaries (`OutageSwitch::set_hanging`: connects and requests
  never answer): every stage fails over at the 2 s attempt timeout, the call is answered whole.
  No secondaries, the breakers half-open, two calls start together: both are answered whole and
  neither is refused (probation). `resilience.rs` pins the timings exactly (failover of a hung
  connect at 2 s for STT and TTS, a hung recovery ends with the session's error at 2 s), the
  cancel race (text read after its cancel is dropped: no event of the cancelled utterance after
  its `Cancelled` across a recovery, the idle refresh still comes), probation (a probe that
  connects closes the breaker, a second call is admitted, a probe that dies early re-opens it at
  once) and the STT tail (a final lagging its audio by 300 ms: the recovery replays the lag).
- **In the server.** `brisvox_server::resilient::ResilientProviders` holds one
  process-wide `Resilience` and one `Leg` per provider instance; `providers()` builds a
  `ResilientStt` / `ResilientLlm` / `ResilientTts` over them for every call (`IdleRefresh::Auto`),
  with the agent definition's `[stt|llm|tts].secondary` as the secondary leg. A secondary TTS is
  an `OwnVoice`: it replaces the engine's voice id with its own. Test:
  `crates/brisvox-server/tests/resilience.rs` (a hosted websocket call refreshes an idle
  Cartesia-limited TTS session; a failover speaks the secondary's voice).
- **Open.** STT has no idle refresh (the engine streams audio continuously and adapters send
  keepalives); the layer is not in the 100-call chaos suite (its faults would be hidden by the
  recovery the suite then cannot see; `engine_failover.rs` judges it instead).

### Speech-to-speech models as a stage
A speech-to-speech model replaces STT, turn driver, LLM runner, segmenter and TTS with one
`S2sStage` between `AudioIn` and `TransportOut`; epochs, the clear hook, the playout book, the tool
engine and the bounds are kept. Design, the turn-detection decision, the truncation rule and the
failure policy: `docs/S2S.md`.
