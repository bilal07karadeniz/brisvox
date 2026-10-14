# Turns and interruptions

## Turns and epochs

Every user turn gets a number, its **epoch**. The turn driver is the only component that
advances it. Bot-side frames (LLM text, TTS audio, playback marks) carry the epoch they were
produced for; every stage on the bot side **drops frames from a stale epoch**. This is the one
mechanism that keeps a late audio chunk, a late LLM token or a late tool result from leaking into
the next turn.

## Deciding that the caller has finished

A pure state machine (`TurnMachine`) with injected time decides when a turn ends. The policies:

- **Adaptive (default).** When the VAD sees the caller fall silent, two things start at once:
  the STT `Finalize`, and the Smart Turn model (an on-device end-of-turn classifier, run on the
  inference pool). If the model says the utterance is complete, the turn ends as soon as the STT
  final has arrived; if it says the caller is not done, the turn waits, with a fallback deadline
  (`fallback_silence_ms`) so an undecided pause cannot hang the call.
- **Fixed.** The turn ends after a fixed silence.

Because the turn end waits for the STT final (a "finals gate", bounded by a timeout), the added
wait is the larger of the two, not their sum.

With **speculation** enabled, the LLM request is started at a candidate end of turn and
committed only when the turn really ends; a read-only tool may run on a speculation, a
side-effect tool is held until the commit. What this buys, and what it costs in extra requests,
is measured in `bench/results/speculation-fixtures.md` (summarised in
[Benchmark results](../benchmarks.md)).

## Barge-in

When the caller starts speaking while the bot is speaking, the turn driver judges the candidate
interruption (the interruption policy: minimum words, backchannel lists such as "yeah" or
"uh-huh" per language, an echo guard) and, if it is real:

1. starts the new epoch (an `Interrupt` always precedes the new `UserTurnStarted`);
2. sends `Clear` straight to the transport on a fast path, so queued audio is dropped and the
   client is told to flush what it buffered;
3. cancels the LLM round, the segmenter and the open TTS contexts;
4. cuts the context to what was actually played
   ([Played-text truncation](played-text.md)).

The budget for step 2, decision to `Clear` applied, is 1 ms at p95. After it, no stale audio
reaches the wire.

Read more: "Turn-taking" in [Architecture](../reference/architecture.md) (the state diagram and
the policies), sections 2 and 3 of the [pipeline design](../reference/pipeline.md) (epoch
ownership and the barge-in path step by step), and the interruption keys in
[Configuration](../configuration.md).
