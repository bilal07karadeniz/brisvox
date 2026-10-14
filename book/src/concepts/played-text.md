# Played-text truncation

When a caller interrupts, the LLM has already produced (and the TTS may have synthesised) more
text than the caller ever heard. If the context kept the whole answer, the model would believe it
had said things the caller never heard, and the next turn would be built on a conversation that
did not happen.

brisvox's rule: **after an interruption, the LLM context contains only what was actually
played.**

## How it is known what was played

The transport reports what reached the caller:

- **Marks.** The engine ends every utterance with a mark; the client echoes it when the audio
  before it has played. A utterance whose end mark came back is kept whole.
- **Playback positions.** For a cut utterance, the transport reports how much audio it had
  released to the wire. The heard position is that position minus the client's playback lead
  (reported by the carrier where it can, e.g. Twilio, else a configured margin).
- **Word timestamps.** TTS providers that return word timings let the position be mapped to an
  exact word. For providers without them (for example Deepgram Aura) the engine falls back to a
  character-rate estimate and the cut is coarser.

The engine waits a short, clock-bounded time for these reports to settle (250 ms by default,
counted from the barge-in), then cuts the assistant message at the heard word. Orphaned tool
results are removed with the message that requested them, and the result is validated: a context
that would break the LLM's message rules is a test failure, not a runtime surprise.

The text that was sent to the TTS is tracked as exact slices of the assistant text, so the
cut is byte-accurate against what the LLM wrote, even when the TTS normalised numbers or
abbreviations.

Read more: section 5 of the [pipeline design](../reference/pipeline.md) (sources of truth and
the procedure) and "Played-text truncation" in [Architecture](../reference/architecture.md).
