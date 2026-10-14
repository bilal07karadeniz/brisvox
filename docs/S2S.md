# Speech-to-speech models as a stage

A speech-to-speech (S2S) model takes the caller's audio and answers with audio, calling tools in
between. One S2S session therefore replaces the STT, turn driver, LLM runner, segmenter and TTS
stages of `docs/PIPELINE.md`. This file is the design of that replacement and the reference for the
code (`crates/brisvox-providers/src/s2s.rs`, `crates/brisvox-providers/src/openai_realtime/`,
`crates/brisvox-engine/src/s2s/`). Facts about the first provider are in
`docs/providers/openai-realtime.md`.

Status: OpenAI Realtime adapter and the stage are implemented and tested against mocks. Not done:
live runs with a key, and the follow-ups at the end. The
Gemini Live adapter (`crates/brisvox-providers/src/gemini_live/`, facts in
`docs/providers/gemini-live.md`), re-seeding of a new session from the local mirror and the
rotation that survives the provider's session limit (section 7a) came later.

## 1. The graph

```
classic:  AudioIn -> SttStage -> TurnDriver -> LlmRunner -> SegmenterStage -> TtsStage -> TransportOut
S2S:      AudioIn -> S2sStage ------------------------------------------------------> TransportOut
```

`AudioIn` (resample to the model rate, optional local VAD) and `TransportOut` (pacing, marks,
playout book) are unchanged. `S2sStage` speaks to the transport stage in the same frame
vocabulary as `TtsStage` (`UtteranceStarted`, `Audio` at the call's output rate, `TtsEnded`) and
to `TransportOut`'s upstream feed (`TtsStarted`, `TtsEnded` when the client played the end mark), so
the pump, the marks, the position reports and the clear path all work as they do for TTS audio.

Assembly: `SessionBuilder::new_s2s(config, clock, timeline, offload, provider, S2sOptions)` (the
three classic provider type parameters are `Unused`), or `with_s2s(provider, options)` on any
builder. `build_session_pipeline` builds the classic stages anyway for their counters and the shared
`Context`, and only puts `S2sStage` in the pipeline. `SessionHandles::s2s` carries the stage's
counters. The system prompt is the model's instructions, `with_voice` its voice, `with_tools` its
tools. Ignored in S2S mode (documented on `with_s2s`): smart turn, interruption policy, flows,
summarizer, filler and phrases, truncation wrapper, text/say input, conversation edits, speculation.

## 2. The provider-neutral interface

`brisvox_providers::s2s`, shaped like `SttSession`/`TtsSession`:

- `S2sProvider::connect(S2sConfig) -> S2sSession { audio, control, events }` (+ `warm`).
- `S2sConfig`: instructions, voice, tools (`ToolSpec`), `S2sTurnDetection::{Provider, Manual}`,
  input/output rate, input transcription, language.
- Audio in: PCM16 mono `Bytes` frames on a bounded channel (the engine uses `try_send`).
- `S2sControl`: `Commit`, `ClearInput`, `CreateResponse`, `CancelResponse`,
  `Truncate { item, audio_end }`, `ToolResult { call_id, output }`, `Note { text }`.
- `S2sEvent`: `SpeechStarted/Stopped`, `ResponseStarted`, `Audio { response, item, pcm }`,
  `AudioDone`, `AssistantTranscript`, `UserTranscript`, `ToolCall` (once per `call_id`),
  `ResponseDone { status }`, `Warning`, `Error` (fatal, last).

What a provider cannot do fails `connect`; nothing is emulated silently. Gemini Live maps onto the
same trait (additive extensions: `S2sConfig::history` with `S2sHistoryItem` to seed a new
session, `S2sProvider::session_limit`, `S2sEvent::SessionEnding`). Where Gemini has no equivalent of
a control it is documented in `gemini_live/mod.rs` and `docs/providers/gemini-live.md`: `Truncate`
is accepted and ignored (the server cannot cut its history to the heard share; the mirror is
exact), `CancelResponse` ends the response locally and the next `activityStart` interrupts the
server, `CreateResponse` after `Commit` or a tool result is absorbed because the server answers by
itself, response and item ids are minted by the adapter.

## 3. Who decides when the caller's turn ends (decision)

Two modes, `S2sTurnMode`:

- **Provider (default).** The provider's VAD (`server_vad`/`semantic_vad`) ends turns, creates the
  response and cancels a running one. Audio is streamed continuously (the VAD needs the silence).
  The barge-in decision is the provider's `SpeechStarted`. Why default: it is the lowest latency
  (no hop to our VAD, the provider's turn detector is tuned for its model) and needs no local model.
  Cost: one provider-VAD false trigger (echo, noise) cuts the bot, and the decision cannot use our
  Smart Turn model or the transcript-based echo guard.
- **Local.** The provider's VAD is off (`turn_detection: null`). The call's local VAD
  (`with_vad`) decides: `UserStartedSpeaking` is the barge-in decision, `UserStoppedSpeaking`
  commits the buffer and creates a response. Audio is sent only while the caller speaks plus a
  300 ms pre-roll, so the provider's buffer holds one utterance. Why: our VAD thresholds, the
  same VAD as the classic pipeline (so behaviour and tests carry over), no provider VAD cost.
  Not included yet: Smart Turn and the finals gate (needs transcripts), i.e. the end of turn is the
  VAD's minimum silence.

In both modes barge-in is VAD-only: the backchannel and echo rules of `InterruptionPolicy` need STT
words, which an S2S call does not have until the provider's transcript arrives.

## 4. Epochs and the barge-in path

`S2sStage` is the sole epoch authority, like `TurnDriver`: the epoch moves at every start of caller
speech. If the bot was busy at that moment (a response generating or requested, a tool running, or
audio of the epoch not yet reported played by the client) it is a barge-in:

1. `hook(new_epoch)`: `note_clear` on the `PlayoutBook`, then the transport `Clear` (fast path), the
   same `clear_hook` as the turn driver (bounded by `timeouts.send`).
2. `Interrupt{new_epoch}` downstream; `TransportOut` marks `AudioOutStopped` and drops stale frames.
3. The stage's own queued audio is discarded (counted); framer reset; current response forgotten
   (later events of it are dropped as stale and counted); tools of the old turn are cancelled
   (`CancellationToken`); in Local mode `CancelResponse` is sent (the provider's VAD does it itself
   in Provider mode).
4. The truncation job starts (section 5). It never delays 1 to 3.

Frames of a response carry the epoch current when `ResponseStarted` arrived. The epoch is
per user turn: a tool round's second response is in the same epoch, so the transport's "Clear before
the first audio of a newer epoch" rule cannot cut the first answer's audio. `TtsEnded` (the end
mark) is sent once, when nothing more is coming: no response generating or requested, no tool
running.

## 5. What was played: truncation

The model keeps its own history, so "after an interruption the context contains only what was
played" is enforced there with `S2sControl::Truncate { item, audio_end }`. On a WebSocket the
server cannot know what the caller heard (it generates far ahead of real time; audio waits in our
queue, the transport's queue and the client's buffer), so the client must say.

`audio_end` is computed exactly like the played-text truncation does, from the `PlayoutBook`: the
stage registers one round and one utterance per assistant audio item (`RoundKey(epoch, n)`); the
pump writes positions and mark echoes; `TransportOut` adds the audio handed over. After the clear
the job waits for the book to settle (`wait_settled_through`, bounded by `settle_wait`), then
`snapshot(key)`: an item whose end mark was echoed (or that a later one followed) is whole; else
`heard_position(lead_margin)` = min(released, total) minus the client's lead, clamped to the audio
received for the item. An item heard to within one frame of everything is not truncated. The cut
text is estimated for the local `Context` mirror (transcript prefix at a word boundary in the heard
share; the model's own history is cut with the exact duration). An item whose round the book no
longer knows is left alone, never cut to zero.

Ordering: the provider's own VAD may create the next response before the truncate arrives. The job
starts at the barge-in and the caller's speech takes hundreds of milliseconds, so the truncate
wins in practice; in Local mode the commit waits for the job (bounded), so it always wins. Accepted
risk in Provider mode, to be measured in a live run.

## 6. Tools

Tool calls arrive as `S2sEvent::ToolCall` and run through the call's `ToolEngine` when their
response is done (`ResponseDone(Completed)`) and the turn is still current. This reuses the engine's
semantics unchanged: read-only tools are cancelled by a barge-in (`interrupted`); side-effect
tools never run speculatively, never run twice (idempotency key `(session, epoch, call id)`, plus the
orphan window), and report late results as notes (`S2sControl::Note`, flushed before the next turn).
Each call gets a stage-minted `ToolCallId::new(epoch, seq)` with a session-wide `seq`; the provider's
`call_id` is mapped back for the answer. A call id seen before is ignored (the adapter and the stage
both de-duplicate).

Answers go back as `ToolResult` (a `function_call_output` item). Then, only if the turn is still
current, `CreateResponse`. A result of a superseded turn is added to the history and no response is
requested (the caller is already talking). Calls of a cancelled or superseded response are not run
and are answered `interrupted`, so the model's history stays well formed. Limits: calls per
response (16), tool rounds per turn (`llm.max_tool_rounds`), the engine's own argument and result
bounds. Past the round limit the calls are answered `tool_round_limit` without running, the model
gets a note ("answer without tools") and one `CreateResponse` per turn, so the caller does not hear
silence; a model that keeps calling tools after that is not asked again. The same note goes out when
the `ToolEngine` reports an exhausted retry budget. A response that ends `Failed`, `Cancelled` or
`Incomplete` has its calls answered `interrupted` and no new response is requested (a failure is
reported as a non-fatal error frame).

## 7. Bounds, timeouts, failure

- Caller audio: stage to task queue 64 frames, drop-newest, counted; task to provider 64 frames,
  drop-newest, counted; Local mode keeps a 300 ms pre-roll ring. Commands: 64, counted if lost.
  Local-mode commands carry the number of audio chunks queued when they were issued, and the task
  takes that audio from its queue first, so the end of an utterance is not clipped by a command
  that overtakes queued frames. The adapter sends the audio queued before a `Commit` ahead of it.
- Liveness: the OpenAI adapter pings after `keepalive_interval` without any inbound frame and
  fails the session with a timeout error when no frame (Pong) arrives within `io_timeout`; the
  stage then runs its usual lost-session path (settle, reconnect with backoff).
- Provider events: 128, backpressure on the adapter's socket reader. Output audio: a `VecDeque` in
  the task, bounded by `S2sOptions::max_buffered_audio` (default 60 s); beyond it chunks are dropped
  and the utterance is marked lost (counted). The select loop never blocks on the downstream: a
  barge-in empties the queue first.
- Every network wait has a timeout: connect (`timeouts.connect`, and the adapter's `io_timeout` for
  the handshake, each setup answer, each write and the event hand-over), the clear and interrupt
  sends (`timeouts.send`), the resampler build (pool, `timeouts.join`), the response wait
  (`response_deadline`).
- A dead session (error, closed channels, stuck control queue) is replaced with a doubling backoff
  (`reconnect_backoff`, cap 5 s) up to `max_reconnects` consecutive failures (restored after a
  10 s session). The model's history is lost with the session, so the new session is seeded from
  the local mirror (7a). When the budget is spent the stage emits one fatal error frame (an
  `EngineEvent::Error` with `fatal`) and drops caller audio, counted. The task is a `TaskGuard`: it
  dies with the stage.
- No `unwrap`/`panic`/indexing in the library code; time only through the injected clock.

## 7a. Re-seeding and rotation

**Seed.** Every connect (first, reconnect, rotation) builds `S2sConfig::history` from the stage's
local `Context` mirror (`s2s/seed.rs`): caller transcripts, the assistant text that was actually
played (cut at a barge-in, so the invariant "the context holds only what was played" carries over
to the new session), tool calls with their results, late-result notes. Leading system messages
(the instructions) are skipped because the configuration carries them. A tool call without a result
(running or interrupted) and a result without its call are left out: providers refuse both.
Bounds: the newest 128 items, 32000 characters in total, 4000 characters per text (a larger tool
result is cut; arguments that are too large become `{}`). Caller turns are in the mirror only through the provider's
transcripts, so the stage requests them whenever the provider has a `session_limit` (rotation),
whatever `S2sOptions::input_transcription` says; for a provider without a limit a reconnect seed
holds the assistant's text but not the caller's unless the option is set. The OpenAI adapter sends one
`conversation.item.create` per item after `session.updated`; the Gemini adapter one `clientContent`
(see the provider docs for what is UNVERIFIED).

**Rotation.** A provider session has a limit (`S2sProvider::session_limit`: OpenAI Realtime 60
minutes, Gemini Live about 10 minutes per connection). `S2sStageConfig::rotate_margin` (default 2
minutes, never more than half the limit) before it, or at once when the provider announces the end
(`S2sEvent::SessionEnding`, Gemini `goAway`), the task connects a replacement seeded from the
mirror while the old session keeps serving. It switches at the first moment that is quiet: no
response, tool or playback, the caller silent, and the mirror still the length it had when the seed
was taken. A due rotation re-checks every 250 ms while the bot is busy; a replacement that arrives
at a busy moment is dropped (`rotations_deferred`) and retried; a failed connect is retried with
the reconnect backoff (`rotation_failures`). The old session is closed by dropping its channels.
The call hears nothing, no error frame is emitted and `session_failures` stays 0. If the session
dies before the switch (limit reached while the bot was busy, a network error) the ordinary
reconnect applies, seeded the same way; only the response in flight at that moment is lost.

**Guards.** (1) A seeded session that dies within 5 s of opening is taken to have refused its seed
(Gemini closes the socket on a history it dislikes): the next session opens without history; if
that one outlives the grace until its own rotation the stage stops seeding for the rest of the
call, otherwise the seed was not the cause and seeding resumes. (2) Provider speech events are not
trusted alone for "the caller is silent": loud caller audio (mean amplitude 500 or more) within the
last 700 ms also blocks the swap, so a provider that sends no speech-start event is not rotated
mid-utterance. (3) The Gemini mapper ignores an output-transcript chunk that arrives after the
turn ended instead of opening a response for it.
Counters: `rotations`, `rotations_deferred`, `rotation_failures`.

## 8. Tests

- Adapter contract (`crates/brisvox-providers/tests/openai_realtime.rs`, mock Realtime WebSocket
  server on a loopback socket): bearer auth and no beta header, `session.update` shape, base64
  audio, every control event, event mapping, a tool call delivered once from either event,
  `function_call_output` before `response.create`, barge-in truncation (`conversation.item.truncate`
  with `audio_end_ms`), racy errors swallowed and other errors surfaced, setup rejection and setup
  timeout, a lost connection, a malformed message.
- Stage (`crates/brisvox-engine/tests/s2s.rs`, a whole call over the loopback transport with a
  scripted mock provider, paused time): session configuration, one epoch and one end mark,
  barge-in clear with no stale audio and truncation to the played duration, superseded-response
  events dropped, queue discard, tool round trip, side-effect de-duplication, superseded tool
  results, local turn detection and local barge-in, reconnect, refused connect, connect timeout,
  budget exhaustion, bounded output queue. Fault-injection mutations of each (the truncation send,
  the stale check, the end-of-epoch rule, the clear hook, the interrupt, the queue discard, the
  tool-result response rule, the call de-duplication) fail at least one test.

## 9. Open items

- Live run with an OpenAI key: real latency (voice-to-voice, barge-in to silence), Turkish quality,
  the truncate-versus-next-response race in Provider mode, `session.update` fields marked UNVERIFIED
  in the provider doc (input transcription block, G.711 formats, idle behaviour).
- Live run with a Gemini key: Gemini Live latency and Turkish quality, the UNVERIFIED items
  of `docs/providers/gemini-live.md` (tool schema field, `clientContent` mid-session, empty
  `turnComplete` as a response request, binary frames, `voiceActivity` by default, header key), and
  what OpenAI does at the 60 minute limit and with an assistant `output_text` seed item.
- Gemini session resumption handles (keep the audio-native context across the 10 minute connection
  limit) instead of a text re-seed; only if the live run shows the re-seed is audibly worse.
- Smart Turn and a transcript-aware echo guard in Local mode; a greeting (the model speaks first).
- Telephony formats without double resampling (G.711 passthrough, a 24 kHz `AudioIn` target).
- `brisvox-server` config switch and `docs/CONFIG.md` entry for choosing an S2S provider.
