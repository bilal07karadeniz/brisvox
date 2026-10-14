# The pipeline

A call is one **session**. The production pipeline (`brisvox-engine`, `run_call`) is a chain of
stages connected by bounded queues; each call has its own chain, so calls share nothing but the
inference pool and the provider connections' budgets.

```text
transport -> AudioIn -> VadWorker -> SttStage -> TurnDriver -> LlmRunner -> SegmenterStage -> TtsStage -> TransportOut -> transport
```

| Stage | Job |
|---|---|
| `AudioIn` | Resamples the call's audio to 16 kHz and forwards it at once. A copy goes to the VAD through a bounded, drop-oldest queue, so the audio path never waits on a model. |
| `VadWorker` | Voice-activity detection (Silero) as jobs on the dedicated inference pool. Emits backdated speech start/stop events. |
| `SttStage` | Streams audio to the speech-to-text provider; at the VAD stop it sends `Finalize` immediately, in parallel with the end-of-turn model. |
| `TurnDriver` | The single authority over **turns and epochs**: end-of-turn decisions (silence, Smart Turn, the STT final), barge-in judgement, the fast `Clear` towards the transport. |
| `LlmRunner` | One task per turn: streams the LLM, runs the tool loop, applies [played-text truncation](played-text.md) to the context. |
| `SegmenterStage` | Cuts the streaming text into speakable chunks; the first clause goes to the voice after one word. |
| `TtsStage` | Streams text to the text-to-speech provider and cuts audio into 20 ms frames at the call's rate. |
| `TransportOut` | Paces audio onto the transport, tracks what was actually played (marks, playback positions) and executes `Clear`. |

A speech-to-speech model can replace everything between `AudioIn` and `TransportOut` with one
stage; see [Speech-to-speech models](../reference/s2s.md).

## Rules the pipeline lives by

- **No heavy work on async workers.** Anything heavier than about 50 microseconds (VAD, the
  end-of-turn model, resampling in bulk) runs on the dedicated `brisvox-inference` pool.
- **Every queue is bounded and has a documented overflow policy.** Audio on the way in is
  lossy and counted (drop-oldest); control events are lossless; audio towards the caller applies
  backpressure.
- **Every network wait has a timeout and every task is cancellable.** Cancellation leaves state
  consistent.
- **Time comes from an injected clock.** That is what makes the simulations deterministic: a
  whole call runs on a paused clock, with mock providers, in milliseconds of real time.
- **Latency is a budget, and it is measured.** The engine's own added time is budgeted at 3 ms
  (turn end to LLM request), 2 ms (TTS audio to transport) and 1 ms (barge-in decision to
  `Clear` applied), at p95. See [Benchmark results](../benchmarks.md) for the measured values.

Read more: [Production pipeline design](../reference/pipeline.md) (stage by stage, with the
queues and the failure policy) and [Architecture](../reference/architecture.md) (crates, frame
flow, routing).
