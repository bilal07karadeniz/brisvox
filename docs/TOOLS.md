# Tool-calling contract

1. **Schema**: every tool declares name, description, JSON Schema for args, `kind`
   (`ReadOnly` | `SideEffect`), timeout, optional filler policy.
2. **Normalization**: one internal representation; per-provider adapters translate
   (OpenAI, Anthropic, Gemini, OpenAI-compatible). Tool-call ids are stable internally.
3. **Assembly**: streamed argument deltas are buffered until complete; partial JSON never executes.
4. **Validation**: args validated against schema before execution. Invalid → structured error
   result to the LLM; bounded retries (default 2), then graceful spoken fallback.
5. **Execution**: independent calls run in parallel; results appended to context in the
   order the LLM issued them.
6. **Idempotency**: key = hash(session, turn epoch, call id). A SideEffect call with a seen key
   returns the recorded result instead of re-executing. A model that repeats a SideEffect call it
   already got the result of, in a later response of the same turn (a new call id), is refused
   `already_done` unless the tool is `repeatable`. The scope of this guarantee is exactly
   that, see "What "never twice" covers" below.
7. **Speculation**: SideEffect calls from speculative LLM runs are held until the turn commits;
   discarded if the speculation is cancelled. ReadOnly calls may run speculatively.
8. **Interruption**: ReadOnly in-flight calls are cancelled. SideEffect in-flight calls complete,
   are recorded, and are reported to the LLM on the next turn.
9. **UX**: after the filler delay (policy default 1.2 s, or the tool's `filler_after`) without a
   result, play a filler phrase (pre-rendered when possible). Long tools may stream progress
   messages.
10. **Observability**: every call traced with args (redacted), duration, outcome, retries.

## What "never twice" covers

The guarantee is per call (one phone call or session), per turn. Exactly:

| Situation | What happens |
|---|---|
| The same tool call (same session, turn epoch and call id) is delivered again: a replay, a redelivery over the agent link | The recorded result is returned; the tool does not run again. If the first run is still in flight the answer is `duplicate_call`; if its outcome is unknown, `unknown_outcome`, and it is not run again. |
| The model repeats an identical SideEffect call (same tool, same arguments) in a **later response of the same turn**, after it read a successful result | Refused as `already_done`, unless the tool is `repeatable` (the built-in `send_dtmf` is). |
| A SideEffect call was **cut off** (barge-in, timeout) and the model calls the same tool with the same arguments again within `orphan_window` (default 120 s) | Not run again: it gets the recorded result if the first run finished, or a note that it was reported (`orphan` handling). |
| Two identical calls inside **one response** | Two calls: both run ("book two tables for Kaya"). |
| The same request in a **later turn** of the same call (the caller asks again) | A new turn epoch is a new key: it runs. The engine cannot tell a deliberate second request from a repeat. |
| The same customer **phones again** | A new session with an empty store: it runs. |
| The server restarts, or another instance handles the call | The store is in memory and per session: nothing is remembered. Do not rely on the key being unique across restarts or servers. |
| An old result was evicted (more than 256 entries or 4 MiB in one session) | A replay of the evicted key runs again. |
| A tool held for confirmation (`confirm`) | Takes no part in the repeat and cut-off rules above: it runs once, when the caller confirms. |
| A SideEffect tool returned an error | Recorded like a success: a replay does not re-run it. A retry needs a new call id. |

So "never twice" means: the engine will not run the same tool call twice, and will not let a model
that already read a success run the same call again in that turn. It does **not** enforce a
business rule such as "one refund per order, ever" or "one booking per slot". For those, **use an
application idempotency key** that is derived from your business data and is the same across
calls and turns, for example the order id or `sha256(customer, slot)`:

- enforce it where the effect lives: a unique constraint in your database, or the `Idempotency-Key`
  header of the downstream API (payment and booking APIs support one), and make the tool return
  the existing result (`already_done`, the old booking) when the key was seen;
- `ctx.idempotency_key` / `ctx.idempotencyKey` in the SDKs (and `ctx.idempotency_key().digest()`
  in Rust) is still the right key for the *redelivery* of one call: pass it along as well;
- the examples show both: `examples/booking-agent` (a store with its own guard) and
  `examples/rust-tools`.

## Internal representation
`brisvox_agent::context`: `Message` = `System` | `User` | `Assistant { text, tool_calls,
provider_data }` | `Tool { call_id, content, is_error }`.
- Ids: internal and stable, `t<epoch>.<seq>`, where `seq` counts every call of the turn across
  tool-loop rounds and restarts — mint them with `Context::next_tool_call_id(s)` (the tool-call
  assembler owns this), so item 6's idempotency key (session, epoch, call id) never maps
  two calls to one result. The provider's id stays in `ToolCall::provider_id`.
- Ordering (`Context::validate`): every call answered exactly once, after its assistant message
  and before any other message (system included); results in any order among themselves; no
  empty assistant message (drop it, e.g. after truncation to nothing played).
- Interruption with calls in flight (item 8): `Context::interrupt_pending_tool_calls` closes
  them with an "interrupted" error result so the next turn is valid; a SideEffect call that
  finishes later is reported by `Context::push_late_tool_result` as a system note.
- Provider round-trip data (reasoning signatures etc.) rides in `provider_data` on the
  assistant message and on each call; adapters define and verify its contents.
- Token estimates go through `TokenEstimator` (default `CharsPerToken`: bytes/4 + 4 per
  message). Serde form: `{"role": "...", ...}`; arguments must be a JSON object (adapters check).

## Assembly
`brisvox_agent::assemble`: a provider-neutral `ResponseAssembler` fed with `Delta` (`Text`,
`ProviderData(value)` for the whole response, or `ToolCall { index, id, name, arguments,
provider_data }`). Adapters translate their stream events into deltas; the agent never depends
on `brisvox-providers`.
- Text is appended in order. Fragments are buffered per `index`; fragments of different calls
  and text may interleave; JSON may split anywhere (inside strings, escapes, numbers).
- `id`/`name` may arrive on any fragment: the first non-empty value wins, a different later
  value is an `AssembleError` and the delta is not applied. Empty strings count as absent.
- Nothing is parsed while streaming. `finish(epoch, &Context)` parses once: ids are minted with
  `Context::next_tool_call_ids` in index order, empty or blank arguments mean `{}`, and a call
  with no name, invalid JSON or a non-object becomes `Assembled::Invalid` (its id, raw text and
  `CallError`), answered with `InvalidCall::error_result()`. Valid siblings still assemble.
- Buffered bytes are capped (default 256 KiB, `with_max_bytes`); a delta over the cap is an
  error, and each new call slot costs 64 bytes so empty fragments cannot grow the buffer forever.
  Provider data is charged at its serialized size.
- Calls per response are capped (default `DEFAULT_MAX_CALLS` = 32, `with_limits(bytes, calls)`).
  A fragment that would open a call beyond the cap is `AssembleError::TooManyCalls` and is not
  applied; fragments for calls already open are still accepted. The cap bounds the fan-out of
  one LLM response (every call is executed and answered, SideEffect ones included), so the
  tool engine can dispatch a response's calls without a further limit.
- Provider data: `Delta::ToolCall { provider_data }` is kept per call (first non-null value wins;
  a different later value is `ConflictingCallProviderData`), and `Delta::ProviderData` is kept
  per response (`ConflictingResponseProviderData` on a different later value). Both are carried
  verbatim into `assistant_message()`, invalid calls included, so an adapter can echo a
  signature back. Send each value whole on one fragment: it is not concatenated.
- Cancelling is dropping (`cancel(self)`): a cancelled response yields no text and no tool calls.
  The guarantee is structural: `cancel` consumes the assembler, and a doctest checks that
  `finish` cannot be called afterwards.
- `Response::assistant_message()` builds the assistant `Message` for the context (`None` when
  there is no text and no call; provider data alone does not count). A named invalid call is
  recorded under its name with `{}` arguments, and `unknown_tool` is used when no name arrived,
  so the context stays valid and the error result explains. Pinned by tests that read the
  recorded message, not only the error results.
- Caller obligations: push a response's assistant message before finishing the next response of
  the same epoch. Ids come from the calls already in the context, so two responses finished
  against the same context before either is pushed get the same ids, and `Context::validate`
  rejects the second push (pinned by a test).
- Cost: `finish` parses linearly in the buffered bytes. Run it on `brisvox-inference` for
  responses near the cap, not on a tokio worker.
- Decision: `id`/`name` take the first non-empty value. The literal "first non-None"
  reading would keep `Some("")` and raise `ConflictingId` on the real id that follows, which
  breaks providers that repeat empty fields on every chunk.
- `warm()` is not part of the assembler. It stays on `LlmProvider::warm` (default no-op) and is
  wired by the connection pre-warming code that knows the provider adapters. This keeps `brisvox-agent` free of a dependency on `brisvox-providers`.

## Tool engine
`brisvox_agent::tools` (`tools.rs`, `tools/schema.rs`, `tools/store.rs`). Implements items 1, 4,
5, 6, 7, 8 and 10 of the contract. Item 9 (fillers) is carried here and played by the runner
under the policy of "Tool UX" below.

**Dependencies.** `jsonschema` 0.58.6 (MIT), built with `default-features = false`: draft
2020-12 validation, no network, no remote `$ref` retrieval. It adds 36 packages to the lock
(`referencing`, `regex`, `fluent-uri`, ...). One transitive license is MIT-0 (`borrow-or-share`,
via `referencing` -> `fluent-uri`), allowed by a single-crate exception in `deny.toml`. Schemas
compile once, at registration. `brisvox-agent` enables tokio's `rt` and `sync` features (SideEffect
calls run on their own tasks). `tracing-subscriber` is a dev-dependency (the trace test).

**Specs.** `ToolSpec { name, description, parameters, kind, timeout, filler_after }`, built with
`ToolSpec::new(name, description, parameters, kind)` plus `with_timeout` / `with_filler_after`.
- `kind`: `ToolKind::ReadOnly` or `ToolKind::SideEffect`.
- `repeatable` (default `false`, `with_repeatable(true)`): a SideEffect tool whose identical call
  may repeat in later responses of one turn (see "Repeats within a turn").
- Defaults: `timeout` = `DEFAULT_TOOL_TIMEOUT` (5 s); `filler_after` = `Some(DEFAULT_FILLER_AFTER)`
  (700 ms). The engine does not play fillers: the `FillerPolicy` reads `filler_after` (see "Tool UX
") and the runner plays the phrase.

**Registry.** `ToolRegistry::register(spec, tool)` rejects, with `RegistryError`:
- `InvalidName`: not 1 to 64 ASCII letters, digits, `_` or `-`.
- `Duplicate`: the name is taken.
- `ZeroTimeout`.
- `ConfirmNeedsSideEffect` / `RepeatableNeedsSideEffect`: `confirm` or `repeatable` on a ReadOnly
  tool (a ReadOnly tool changes nothing to confirm and may always repeat).
- `InvalidSchema`: parameters without `"type": "object"`, or a schema that does not compile
  offline (for example a remote `$ref`). A local `$ref` (`#/$defs/...`) compiles and validates.

A rejected tool is not added. `specs()` returns the specs in name order (deterministic LLM
requests); `spec(name)`, `len()`, `is_empty()`.

**Tool trait.** `fn call<'a>(&'a self, args: Value, ctx: ToolCallCtx) -> ToolFuture<'a>`, where
`ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>>`.
`ToolOutput::Json(Value)` or `ToolOutput::Text(String)`, with `From` impls for both.
`ToolError::new(message)`. `ToolCallCtx { session, epoch, call, cancel }`, where `cancel` is the
turn's `CancellationToken`, and `idempotency_key()`. Tools receive arguments that already match
the schema.
- The engine's timeout ends the caller's wait. For a ReadOnly tool it also drops the future.
- A SideEffect future is not dropped because of a timeout, an interruption or a dropped `execute`
  future. It keeps running after its slot was answered `timeout` or `interrupted`. A SideEffect
  tool should still bound its own IO with timeouts of its own, because the engine ends the future
  in two cases only: the hard backstop (`max_side_effect_runtime`, below) and the drop of the
  `ToolEngine` (the end of the session), which aborts it at its next await point. A SideEffect
  tool must tolerate being dropped there.
- A tool that panics does not unwind into the caller. A panicking ReadOnly tool (in `call` or in
  its future) is answered `tool_failed` with the detail `the tool panicked`, and its sibling calls
  are answered normally. A panicking SideEffect task is recorded as `Lost` (`unknown_outcome`).

**Validation (item 4).** Each call's arguments must first fit `max_argument_bytes` (default 16 KiB
serialized; checked by a writer that stops at the limit). Larger arguments are malformed
(`arguments_too_large`): they are not validated and do not run, and the round counts toward the retry
budget. Then the arguments are checked against the compiled schema, in the planning step
(synchronous, on the caller's task; see Cost below). Problems are reported as at most
5 issues of `{path, reason}`, with the path as a JSON pointer (`(root)` for the whole object). The
path is bounded to 128 bytes and the reason to 200 bytes, the ellipsis included, plus `truncated`
when more exist. A call with unparseable JSON, a missing name, an unknown tool, invalid arguments,
or a position beyond the call cap is malformed.

**Error results.** Every refusal or failure is a tool message with `is_error: true` and a JSON body
`{"error": kind, "tool": name, "detail": text}` (`tool` at most 64 bytes, `detail` at most 512
bytes, ellipsis included; invalid arguments add `"issues"` and `"truncated"`). Kinds:
- `invalid tool call`: the assembler could not parse it (`InvalidCall::error_result`).
- `malformed`-class kinds: `unknown_tool`, `invalid_arguments`, `arguments_too_large` (serialized
  arguments over `max_argument_bytes`), `too_many_calls` (a 33rd or later call of one response).
- `budget_exhausted`: the turn's retry budget is spent. `stale_turn`: the call's turn was superseded.
- `timeout`, `interrupted`, `tool_failed` (the tool returned an error, or a ReadOnly tool panicked).
- `result_too_large`: a ReadOnly result over `max_result_bytes`.
- `duplicate_call`, `store_full`, `unknown_outcome`: idempotency refusals (below).
- `already_done`: a SideEffect call identical to one that succeeded earlier in the same turn
  (below).

**Engine.** `ToolEngine::new(Arc<ToolRegistry>, SessionId, SharedClock, EngineConfig)`. It must be
used inside a tokio runtime with the `rt` feature, since SideEffect calls are spawned.
`EngineConfig { max_invalid_rounds: 2, max_result_bytes: 64 KiB, idempotency_capacity: 256,
idempotency_max_bytes: 4 MiB, max_argument_bytes: 16 KiB, max_side_effect_runtime: 120 s }`. The
engine raises `idempotency_max_bytes` to at least `max_result_bytes` (so a recorded result is not
evicted by its own recording), treats `idempotency_capacity: 0` as 1, and raises
`max_side_effect_runtime` to at least a tool's own timeout. The engine's entry points:
- `execute(calls, epoch, &cancel) -> Execution`: a committed response.
- `speculate(calls, epoch, &cancel) -> Speculation`: a speculative response (below).
- `commit(speculation, &cancel) -> Execution`: commits a speculation.
- `invalid_rounds(epoch)`, `idempotency_usage()` (`IdempotencyUsage { entries, bytes }`),
  `take_late_notes()`, `late_notes_dropped()`.

`Execution { results, late_notes, budget_exhausted }`: `results` has one tool message per call, in
issue order. `Execution::append_to(&mut Context)` pushes the results, then the late notes.

**Rounds (items 4 and 5).** Every `execute`, `speculate` and `commit` runs the same steps:
1. Plan, synchronously and in issue order. Calls past position 32 of one response are answered
   `too_many_calls`. Malformed calls are answered at once. Under `speculate`, SideEffect calls are
   held. Nothing runs during planning.
2. Admit the round. The engine tracks the newest turn (`epoch`) it has seen.
   - A call of an older turn is stale: every call of the round is answered `stale_turn`, nothing
     runs, and no budget is spent.
   - A newer turn becomes the newest, with a fresh budget.
   - A round with a malformed call adds one to the turn's count. The count does not go down: a
     valid round does not refund it. A round is exhausted when the count exceeds
     `max_invalid_rounds`. An exhausted round runs nothing: every call, valid ones included, is
     answered `budget_exhausted`, and `Execution::budget_exhausted` (or `Speculation::budget_exhausted`)
     is set. Every later round of that turn is exhausted too.
   - A discarded speculation still spends its round, since the LLM was asked.
3. Run. The steps start in issue order, then all of them are awaited together (`join_all`). Each
   result is answered in its own slot, so `results` is in issue order whatever the completion order.
   Two SideEffect calls of one response that share a call id are not both run: the second answers
   `duplicate_call`.

**Speculation (item 7).** `speculate` runs the ReadOnly calls, answers malformed calls and refusals,
and holds the SideEffect calls. `Speculation` exposes `epoch()`, `held_calls()` (the held calls in
issue order), `answered()` (the results already settled) and `budget_exhausted()`. Nothing reaches
the context until commit:
- Commit: `commit(speculation, &cancel)` runs the held calls and returns an `Execution` whose
  `results` are the whole response in issue order (ReadOnly and held results interleaved as the LLM
  issued them). Append it, as for `execute`. Commit spends no budget. A speculation of a superseded
  turn answers its held calls `stale_turn`, and a turn whose budget is spent since then answers
  them `budget_exhausted`.
- Discard: drop the `Speculation`. Its held calls never run. The caller then closes the open calls
  with `Context::interrupt_pending_tool_calls()`, so the context stays valid (`Context::validate`
  would reject a call with no answer before the next user message).

**Cancellation (item 8).** `execute`, `speculate` and `commit` take the turn's `CancellationToken`.
- A call that has not started when the token is already cancelled answers `interrupted` and does not
  run, for both kinds.
- A ReadOnly call still running when the token fires is dropped and answers `interrupted`.
- A SideEffect call still running when the token fires answers `interrupted` at once (outcome
  `interrupted_running`). The call keeps running on its own task. Its result is recorded in the
  idempotency store, and it is reported later as a late note (the same text as
  `Context::push_late_tool_result`, which now covers interruptions and timeouts alike).
- The execution returns when every call has answered. A SideEffect call answers when its result
  arrives, at its timeout, or at the interruption, so `execute` never waits for a running SideEffect
  call past its timeout, and barge-in is not delayed by a booking.

**Timeouts (item 3).** Each call is raced against its tool's timeout on the injected `Clock`.
- ReadOnly: the future is dropped and the call answers `timeout` (outcome `timed_out`).
- SideEffect: the slot answers `timeout` with a detail that says the call keeps running, that its
  result follows as a note, and not to repeat it (outcome `timed_out_running`). The call is not
  dropped: its result is recorded and reported later, so a booking that the provider accepted at
  5.1 s is never reported as failed, and the LLM is not invited to book it again.

**Side effects, idempotency and late results (items 5, 6 and 8).** The key is
`IdempotencyKey(session, epoch, call id)`. `IdempotencyKey::digest()` is a stable FNV-1a 64-bit
value of the same tuple, for logs and provider idempotency headers.
- A SideEffect call runs on its own tokio task, which owns the tool future. Dropping the
  `execute`, `speculate` or `commit` future does not drop it. The task records the result, or, when
  its task ends without one (a panic, an abort), records the key as `Lost`. The engine keeps the
  tasks in a `JoinSet` (reaped on every spawn, so it holds only running calls, which the store
  bounds): dropping the engine aborts them, and each aborted call is recorded as `Lost`.
- Hard backstop (a decision from the tool engine review): the tool future is raced against
  `max(max_side_effect_runtime, the tool's timeout)` on the injected clock (default 120 s). When it
  passes, the future is dropped, the key is recorded as `Lost` (`unknown_outcome`, never run again,
  outcome `lost`), and the call's slot or late note says that the outcome is unknown. So a hung tool
  leaks neither a task nor an in-flight store entry; without the backstop, 256 hung calls would
  have made every later SideEffect call of the session answer `store_full`.
- A SideEffect result is never lost, whatever the caller does. Each result reaches the caller
  exactly once, either in the slot or as a late note:
  - The task sends the result on a oneshot channel. If the channel is closed, the task pushes the
    note itself.
  - The channel's receiver sits in a guard. When the wait ends at the timeout or the interruption,
    the engine closes the channel and takes a result that arrived in the meantime (it answers with
    that result). When the guard is dropped (the `execute` future was dropped), it closes the channel
    and pushes the note for a result that arrived but was never read.
  - A result already read into a slot of a run that is then dropped before it returns (another call
    of the response was still running) is held by the run and pushed as a note on drop.
  So a barge-in `select!` that drops `execute` loses nothing: `Context::interrupt_pending_tool_calls`
  tells the LLM the call was interrupted, and the note that follows tells it what happened.
- Store states per key: in flight, done (the recorded result), lost (the task ended without a
  result). A done key answers its recorded result (`recorded`). An in-flight key answers
  `duplicate_call`. A lost key answers `unknown_outcome` and is not run again. When every entry is
  in flight, a new call answers `store_full`.
- Bounds, per session: at most `idempotency_capacity` entries and `idempotency_max_bytes` of
  recorded result text. The worst case is therefore about 4 MiB per session with the defaults,
  plus the keys. When a bound is reached, the oldest settled entry (done or lost) is evicted. An
  in-flight entry is never evicted. A replay of an evicted key runs again. Call ids are unique
  within a turn, so this needs a retry of an old response.
- A SideEffect tool that returns `Err` is recorded like a success: the key is `Done` with the
  `tool_failed` error as its result (outcome `failed`). A replay returns that recorded error and
  does not run the tool again. A retry needs a new call id (a new LLM response). This is the
  conservative choice: the engine cannot know whether a failed tool changed anything.
- Result size: a SideEffect result over `max_result_bytes` is cut to that size, with the marker
  `...[truncated]`, and it is recorded and reported as done (outcome `ok_truncated`), because the
  effect happened. ReadOnly results over the limit become `result_too_large`. JSON results are
  serialized through a writer capped at the limit, so an oversized value is never serialized in full.
- Repeats within a turn. A SideEffect call whose tool and arguments equal those of an
  earlier call of the **same turn**, of a **different response**, whose result is `Done` and not an
  error, is not run: it answers `already_done` ("an identical call already ran earlier in this
  turn and succeeded; its result is above ... do not repeat this call, tell the caller what
  happened"), counted in `side_effect_repeats`, traced as `repeat_refused`. Found by the
  scenario `S3` (`docs/TOOLS.md`, "Verification"): a model that read the confirmation of its
  `book_table` asked for the same booking again in the next round and the table was booked twice
  (`BK-1002`), because call ids are unique and the idempotency key therefore was too. What is not
  a repeat: other arguments (another booking); the next turn (the caller may ask again); the same
  response (two identical calls of one response are two calls, "book two tables for Kaya"); a call
  whose earlier run failed (a failed SideEffect may be retried, above); an earlier call whose result
  the model never read (an orphan, which the orphan check above answers); a gated call (the
  confirmation gate's own request/confirm cycle); a tool declared `repeatable`
  (`ToolSpec::with_repeatable(true)`: pressing the same IVR digit twice in separate responses; the built-in `send_dtmf` is repeatable, `end_call` and `transfer_call` are not; a tool declared by a remote agent opts out with `repeatable` in its declaration, `docs/AGENT_PROTOCOL.md`, "Remote tools"). The
  arguments compare as JSON values (key order does not matter), through the same digest as the
  orphan check. Cost: the check scans the recent SideEffect calls of the session (at most `ORPHAN_CAPACITY` = 32), under
  the state lock that `start_side_effect` already holds.
- Late notes: a late result that finds the mailbox full (`MAILBOX_CAPACITY` = 64) is counted in
  `late_notes_dropped()` and not reported. Otherwise a late result waits for the next `execute` or
  `commit`, which append it after their results (`Execution::late_notes`), or for
  `take_late_notes()`. Call `take_late_notes()` only when no tool call is open in the context (after
  an `Execution` was appended), so a note never separates a call from its result.

**Stale turns.** The engine refuses a turn once it has seen a newer one in `execute`, `speculate` or
`commit`. A turn that has made no tool call yet is not seen. So the caller cancels the superseded
turn's token when the turn ends, which also stops its calls that have not started.

**Caller obligations.**
1. Push the response's assistant message (`Response::assistant_message`) before `execute` or
   `speculate`. The engine never reads the context.
2. Append each `Execution` (`append_to`) before the next message of the turn.
3. On discarding a speculation, or dropping an `execute` future mid-round (for example in a
   `select!` on barge-in), close the open calls with `Context::interrupt_pending_tool_calls()`. A
   dropped execution's results never reach the context. Its SideEffect calls keep running, and their
   results (including those already finished when the future was dropped) come back as late notes,
   so the caller's barge-in path must not expect them in the dropped execution.
4. Cancel the turn's token when the turn is interrupted or superseded.

**Observability (item 10).** Every call emits one debug event on target `brisvox::tools`: `tool
call settled`, with `session`, `epoch`, `call`, `tool` (at most 64 bytes), `outcome`, `duration_us`
and `retries` (the malformed rounds the turn had spent when the call was admitted). A held call
emits `tool call held` at speculation time. Its settle event follows at commit. A late result emits
`late_result` (from its task, or from the dropped `execute` that held it). Arguments are never
logged (the contract's "args (redacted)" is met by omitting them). Outcomes:
`ok`, `ok_truncated`, `failed`, `timed_out`, `timed_out_running`, `interrupted`,
`interrupted_running`, `not_started`, `recorded`, `duplicate`, `repeat_refused`, `lost`, `store_full`, `malformed`,
`unknown_tool`, `invalid_arguments`, `arguments_too_large`, `too_many_calls`, `budget_exhausted`,
`stale_turn`,
`result_too_large`, `held` and `late_result`.

**Cost (item 4).** Validation runs in the planning step, on the caller's task, not on the
`brisvox-inference` pool, so its cost is bounded by `max_argument_bytes` (16 KiB) instead of the
assembler's 256 KiB: larger arguments are refused before the schema is touched. Valid arguments take
the `is_valid` fast path, which builds no error values. Measured (release build, one run of the
engine's `execute` including the size check and the plan, best of 20, 2026-10-08, development
machine; `uniqueItems` is hash based, not quadratic):

| arguments | bytes | time |
|---|---|---|
| typical (`city`, `days`) | 26 | under 1 us |
| `uniqueItems` array of 2500 integers | 11,399 | 22 us |
| `uniqueItems` array of 1500 strings | 12,399 | 24 us |
| `uniqueItems` array of 800 objects | 7,899 | 45 us |
| 16 KB string against an `enum` | 16,008 | 9 us |
| 1500 strings that all fail a `pattern` (error path, 5 issues reported) | 10,899 | 0.30 to 0.36 ms |
| 40,000 integers (over the cap, refused) | 200,000+ | under 30 us |

Valid and typical calls are inside the 50 us budget of CONTRIBUTING.md. The one case above it is a
hostile, invalid array against a regex schema, bounded at about a third of a millisecond by the
cap. A reason can still quote the offending value, so it is formatted from at most 16 KiB and then
cut to 200 bytes. Moving that error path to the inference pool is not worth a hop for this bound;
lower `max_argument_bytes` for a tighter bound.

**Tests.** `crates/brisvox-agent/tests/tools.rs` (paused time, `SimClock`). Covered:
- schema cases (valid, missing required, wrong type, extra property, nested, a local `$ref`);
  the registry rejections (including a remote `$ref`); bounded validation reports;
- parallel timing (100 ms + 100 ms = 100 ms); issue-order results with reversed completion;
- ReadOnly timeouts and failures; a panicking ReadOnly tool (in `call` and in its future) answered
  `tool_failed` next to answered siblings; the result size limit, exactly at the limit, for Json
  and Text and for both kinds, with exact cut length, kept prefix and multi-byte cuts;
- the argument cap (exactly at the cap runs, one byte over is `arguments_too_large`, a 200 KiB
  `uniqueItems` array is refused);
- ReadOnly interruption (dropped); SideEffect interruption (not dropped, reported later as a note);
  a result sent before a dropped `execute` is read, and a slot resolved inside a dropped `execute`
  (both come back as notes); a multi-thread stress test where tools finish at their timeout (every
  result answered or noted exactly once); the hard backstop (a hung tool is dropped, recorded
  lost, never rerun, and the note says the outcome is unknown); the abort on dropping the engine;
  a call that has not started; a SideEffect past its timeout (keeps running, not repeated, late
  result reported and recorded); a dropped `execute` future (the call is recorded, not stuck); a
  panicking SideEffect task (`lost`, never run again); a SideEffect result over the limit (cut and
  recorded);
- idempotency: a replay runs once, the same call id twice in one response runs once, eviction by
  count, eviction by bytes; an in-flight entry is never evicted and a full store answers
  `store_full` without running; the byte budget raised to one result and capacity 0; a failing
  SideEffect is recorded and not retried; tools receive the right session, epoch, call id and the
  turn's own token; the pinned `digest()` and its dependence on session, epoch and call id;
- repeats within a turn: an identical SideEffect call of a later response of the same turn
  is refused `already_done` and the tool runs once; other arguments, a new turn, two calls in one
  response, a `repeatable` tool, a ReadOnly tool and a retry of a failed call all run; the refusal
  leaves a valid context (mutation-checked: no check, no epoch check, failures counted as
  successes);
- late notes: drained by the next `execute` after the results (valid context), and the mailbox
  keeping 64 and counting the 65th;
- unknown tools; the retry budget (exhaustion, schema failures spending it, no refund by valid
  rounds, a turn that is superseded neither resets nor spends the current turn's budget, and the
  next turn's own budget); the call cap; registry names (valid ones with `_`, `-` and 64 bytes are
  accepted; 65 bytes and non-ASCII are rejected; a duplicate keeps the first tool);
- bounded error fields (path 128, reason 200, tool 64, detail 512 bytes, cut inside multi-byte
  characters, still valid JSON);
- speculation: ReadOnly runs, SideEffect held, commit in issue order (with the booking first), a
  discarded speculation runs nothing and spends its round, a speculation of a superseded turn runs
  nothing on commit; commit spends no budget, answers `budget_exhausted` after the budget ran out
  since the speculation (and for a speculation made under an exhausted budget), answers
  `interrupted` with a cancelled token without running, and a replay returns the recorded result;
- every outcome traced with `session`, `epoch`, `call`, `tool`, `duration_us` (exact on the virtual
  clock) and `retries`, `held` and `late_result` as separate events, and no argument value in any
  field (a capture layer, `brisvox::tools` target);
- `Context::validate()` after every scenario that builds a context; the per-request LLM metrics of a
  three-round tool loop; the engine and its futures are `Send` and `Sync`.

## Tool UX: fillers and interruption semantics
`brisvox_agent::tools` (`tools/filler.rs`, `tools/stats.rs`), and the runner wiring in
`brisvox-engine` (`llm_turn.rs`, `llm_runner.rs`). Implements items 8 and 9 of the contract.

### Filler policy (item 9)
A `FillerPolicy` owned by the tool layer decides **when** and **what**; the `LlmRunner` speaks it.
- **Delay**, measured on the injected `Clock` from the start of the tool round (`execute`, or the
  commit of a speculation): `FillerPolicy::new()` uses one fixed delay, `DEFAULT_FILLER_DELAY`
  (1.2 s), changed with `with_delay`. A tool opts out with `ToolSpec::with_filler_after(None)`; under
  a fixed delay its own `Some(d)` is only the on/off switch. `FillerPolicy::per_tool()` takes each
  tool's `filler_after` as its delay (the original behaviour). `FillerConfig { enabled, delay }`
  (`FillerConfig::DEFAULT` = on, 1.2 s; `FillerConfig::PER_TOOL` = on, each tool's own delay) +
  `from_config` build either.
  A round with several calls fills after the smallest delay of its calls (ties: the first call).
- **Session config**: `LlmConfig::filler` (in `EngineConfig::llm`, default `FillerConfig::DEFAULT`)
  is what the runner builds its policy from, so a session assembled from its config (`run_call`,
  `SimSession`) gets the 1.2 s default and can change or switch it off there.
  `LlmRunnerBuilder::with_filler_policy` replaces the whole policy (phrases, clips) when needed.
- **Why the default is a fixed 1.2 s**: one delay for every tool that has a filler, so
  the caller hears silence for the same time whatever tool runs, and a tool that usually answers
  within about a second (most lookups) does not speak a filler that its answer would cut off, as
  the per-tool 700 ms default of `filler_after` would. Per-tool delays stay available as an opt-in
  (`FillerConfig::PER_TOOL`, `[filler] per_tool = true`) for agents whose tools have very
  different latencies; combining it with `delay_ms` is rejected, since one of the two would be
  ignored. The TTS stage waits for a pending clip before it opens the next provider context
  (step 4 below) because providers stream contexts independently: without the wait, the answer's
  audio could overtake the filler.
- **Phrases**: `with_phrases` (generic list), `with_tool_phrases(tool, ..)`. They rotate
  round-robin on a counter of the policy (no randomness: simulations stay deterministic), and
  only a filler that is actually spoken advances it (`choose` is called when the delay passes).
  A clone starts its own rotation (it shares only the clip cache), so a policy built once and
  cloned into every session keeps each session's phrases independent of the others.
  Default phrase `DEFAULT_FILLER_TEXT` ("One moment, please."). An empty list or
  `with_enabled(false)` switches fillers off.
- **Pre-rendered clips**: a `FillerPhrase` may carry a `ClipKey`. `with_clips(Arc<dyn FillerClips>)`
  names the cache; the key reaches the `FillerChoice` only when the cache reports the clip as
  available (`StaticClips` is the simple implementation; the engine's `FillerClipStore` also holds
  the audio). The runner counts the choices that had a clip (`LlmStats::filler_clips`).
- **Clip playback**: `brisvox_engine::filler_clip`. A `FillerClip` is PCM16 mono at the
  TTS provider's rate with its word timings (`FillerClip::new`, or `with_spread_words` for audio
  rendered without timings); a `FillerClipStore` maps `ClipKey`s to clips, is built once and is
  read-only afterwards. `SessionBuilder::with_filler_clips(store)` gives it to the runner's policy
  (as its cache) and to the `TtsStage` (`with_clips`); `with_filler_policy` sets phrases with clip
  keys. The path:
  1. The runner's filler round cues its `RoundKey` with the clip key in the `PlayoutBook`
     (`cue_clip`) before its `LlmResponseStarted`, then sends the phrase text as usual.
  2. The TTS stage, at the round's first text, finds the cue (`clip_cue`) and the clip in its store
     at the provider's rate, and opens a **clip context**: a context id of the current session (so
     an utterance id, `register_utterance` in the book) that the provider never hears of. The
     round's text is recorded verbatim (`push_sent`) and not sent. A cue it cannot play (no store,
     unknown key, other rate) falls back to the provider (`TtsStats::clip_fallbacks`).
  3. At the round's flush the clip goes to the session's reader on a bounded queue (`CLIP_QUEUE` =
     4; a full queue drops it and marks the utterance lost, `clips_dropped`). The reader plays it
     once no earlier context is left in the ledger, so it comes after the audio before it: the
     same framer path as provider audio (`UtteranceStarted`, frames at the call rate, the `Bytes`
     shared and copied once per piece), then the clip's `Word`s, then the end of the context
     (`TtsEnded` when it was the last one of the response).
  4. A provider context opened while a clip is pending waits for it (`clip_waits`; abandoned at a
     barge-in, bounded by `CLIP_WAIT_LIMIT` = 30 s), because a provider streams contexts
     independently and its audio would otherwise overtake the clip.
  5. Epochs: a clip whose epoch ended is dropped before it plays (`clips_dropped`), its frames
     are dropped like stale provider audio, and an interrupt never sends its context id to the
     provider's cancel channel. A barge-in in the middle of the clip is cut to the heard words
     like provider speech (the played filler stays in the context as its heard prefix).
- **In the runner**: the policy from `LlmConfig::filler`, or `LlmRunnerBuilder::with_filler_policy`;
  `with_phrases(filler, fallback)` replaces the generic phrase of the current policy. The filler is
  its own round (own
  `LlmResponseStarted`, own ordinal in the `PlayoutBook`). Tools keep being polled while the filler
  is sent, so back-pressure does not stall them.
- **In the context, as played**: a spoken filler is part of the conversation and is cut to what was
  heard like any round. It cannot be stored when it is spoken: a message between a tool call and its
  result breaks `Context::validate`, and a trailing assistant message would end the next request on
  an assistant turn (Anthropic rejects that prefill). So the runner keeps it aside
  (`Inner::filler`) and places it as an assistant message of its own **once no call is open**:
  - right before the next assistant message of the turn (the model's next response, or the
    fallback line): `[assistant(calls), tool(result), assistant(filler), assistant(answer)]`. The
    request that produced that answer was built without it (it is placed after it was sent);
    later requests see it (providers merge adjacent assistant messages);
  - or when the turn is settled (`halt` at a barge-in, the next user turn, `Stop`, panic recovery):
    after the open calls were closed with `interrupted` results, and before the user message.

  Its round gets its `msg_index` then (`record_round`), so the played-text truncation cuts it like
  any other: heard in full stays, a prefix is kept, nothing heard removes it (`Removed`). "The
  context holds what was played, and only that" holds for fillers too. Counter:
  `LlmStats::fillers_in_context`.

### Interruption (item 8)
- **ReadOnly**: cancelled. Its future is dropped when the turn's token fires, or when the caller's
  `execute` future is dropped or aborted (the runner aborts its round task at a barge-in). Both are
  counted in `ToolStatsSnapshot::read_only_cancelled`; a call that had not started is
  `not_started_cancelled`. Nothing is reported late and a repeated ReadOnly call is never suppressed.
- **SideEffect**: runs to completion on its own task whatever the caller does, exactly once. Its
  result is recorded in the idempotency store and reaches the context exactly once, as the tool
  result when the caller was still waiting, otherwise as a late note (`side_effect_interrupted`,
  `side_effect_late_results`). The runner appends the note before the next user message, or after the
  results of the next tool round.
- **Regeneration (new)**: the idempotency key `(session, epoch, call id)` cannot recognise a response
  regenerated after a barge-in, because the new turn has a new epoch and new call ids. The engine
  therefore remembers recent SideEffect calls (`ORPHAN_CAPACITY` = 32 per session) with a digest of
  tool name and arguments (key order does not matter). A call is an **orphan** once its result can no
  longer reach the LLM as the tool result of its own turn: interrupted while running, timed out while
  running, its `execute` future was dropped, or its `Execution` was dropped before it reached the
  context (see "Delivery" below). A later call of **another response run** (each `execute` or
  `commit` is one: a later turn, or a later round of the same turn, such as a retry after a timeout
  that the LLM was told not to make) with the same tool and arguments within
  `EngineConfig::orphan_window` (120 s, `ZERO` turns it off) is answered without running the tool:
  - orphan finished: its recorded result, under the new call id (`orphan_replayed`). The orphan is
    consumed, and the late note of the original call is held back from that execution's
    `late_notes` (the LLM reads the result once), only when the execution reaches the context; an
    execution dropped before that leaves the orphan and its note in place, so the next regeneration
    is answered from the record too;
  - orphan finished and its late note already reached the context (taken by `take_late_notes` or
    carried by a delivered `Execution`; for example the barge-in came near the end of the tool and
    the note was appended when the regenerated turn started): `already_reported` ("its result is in
    the note above; do not repeat") (`orphan_reported`). The recorded result is not replayed, so the
    result is in the context once; later repeats within the window are answered the same way;
  - orphan still running: `duplicate_call` ("its result follows as a note; do not repeat")
    (`orphan_in_flight`); the original's note comes once when it finishes;
  - orphan ended without a result: `unknown_outcome` (`orphan_lost`), consumed the same way.

  A call whose result was delivered normally is not an orphan: a user who asks again later runs it
  again. Identical calls of one response are separate calls and both run.
- **Delivery**: an `Execution` carries a `Delivery` guard. `Execution::append_to` (or
  `mark_delivered`, for a caller that wrote the messages itself) marks it delivered. An execution
  dropped undelivered (the runner discards it at a barge-in: the tool round finished while the
  filler was still being sent, or the turn was cancelled right after `execute` returned) never
  reached the LLM, so dropping it reports each SideEffect result it carried as a late note and marks
  the call an orphan, marks its other SideEffect calls orphans, and puts the late notes it took from
  the mailbox back. The result is in the context once (the note), and a regenerated identical call
  is answered from the record instead of running twice.
- Counters: `ToolEngine::stats() -> ToolStatsSnapshot { read_only_cancelled, not_started_cancelled,
  side_effect_interrupted, side_effect_late_results, side_effect_replays, orphan_replays,
  orphan_in_flight, orphan_lost, orphan_reported }`. The runner's `LlmStats` keeps `fillers`, `filler_clips`,
  `interrupted_tool_calls`, `late_notes`.
- Trace outcomes added: `orphan_replayed`, `orphan_in_flight`, `orphan_lost`, `orphan_reported`.

### Tests
`crates/brisvox-agent/tests/tool_ux.rs`: policy (fixed vs per-tool delay, ties, rotation, per-tool
phrases, clip availability, off switches, config); engine (ReadOnly cancelled and counted, not
started, SideEffect completes once and is noted once, a regenerated call replayed or refused while the
first runs, a dropped `execute` future, a timeout orphan, other arguments / expired window / window
off / delivered result are not suppressed, a later round of the same turn is, same-key replay
counted; a replay dropped with its round next to a slow sibling and one in a discarded execution keep
the orphan and its note; a SideEffect result in a discarded execution is noted and not run again;
notes taken by a discarded execution go back; a repeat after the orphan's note was taken or carried
by a delivered execution is `already_reported`, while one after a discarded execution that carried
the note is replayed; a same-turn retry after a timeout does not run again;
two identical calls of one response both run; each clone of a policy rotates on its own).
`crates/brisvox-engine/tests/tool_ux.rs`, through the runner on `SimClock` and `MockLlm`: the filler
at exactly the policy's delay and not before, no filler for a fast tool, phrase rotation with a cached
and an uncached clip, a disabled policy, the 1.2 s default from `LlmConfig::filler` and the per-tool
and off settings of that config, a filler in
the context after the results and before the answer (never in the request that followed it), a
filler cut by a barge-in kept as its heard prefix ("One moment,") through the `PlayoutTruncation`
and one never heard removed, a barge-in before the delay speaking none, ReadOnly cancelled and counted at a
barge-in, SideEffect interrupted then regenerated (the body runs once, one note, one replayed result,
valid context), regeneration while it still runs, an interruption during the filler, a booking that
finished while the filler's frames were back-pressured and was dropped with its execution at the
barge-in (noted once, not run again on regeneration), and a replay cut off with a slow sibling then
regenerated again (the booking still runs once).
`crates/brisvox-engine/tests/tts_clips.rs`, the TTS stage over `MockTts`: a cued clip is its
own utterance after the audio before it and before the next round's (which waited for it), with the
clip's words and book entries and one `TtsEnded`, the provider never hearing the phrase; a missing
clip, a clip at another rate and a stage without a store fall back to the provider; a barge-in while
the clip waits drops it and cancels only the provider's context.
`crates/brisvox-engine/tests/filler_sim.rs`, a `SimSession` on the production pipeline
(`EngineFactory`, audio in, loopback transport): a clip played whole is in the context whole; a
barge-in in the middle of the clip while a 5 s SideEffect booking runs leaves the clip's heard
prefix in the context, the regenerated identical booking is answered `duplicate_call` without
running, and the booking's result reaches the context exactly once (its late note); a 2 s booking
that finishes between the barge-in and the regenerated call (its note is appended when the
regenerated turn starts) is answered `already_reported`, and its result is in the context once.

## Tools in the production pipeline
Where the contract above runs in a call: the `LlmRunner` of `brisvox-engine` (`llm_turn.rs`,
`llm_runner.rs`, `llm_spec.rs`; design and as-built notes in `docs/PIPELINE.md` sections 4 and
12). One round task per turn epoch, on the epoch's cancellation token:
1. The request is built by `RequestBuilder::build` (`build_window`): the tool list is
   name-ordered (prompt-cache stability) and one turn's many tool rounds stay bounded.
2. Text deltas are spoken while they stream; tool-call deltas only go to the assembler (item 3).
3. At `Done` the assistant message, calls included, is pushed to the context **once, before any
   call runs**. With calls, a `Flush` makes the text before the tool speak while it runs.
4. `ToolEngine::execute` races the filler timer (item 9; the filler is its own round). Results are
   appended in the LLM's order, then late notes (only when no call is open), then the next round.
   At most `max_tool_rounds` (6) rounds; past it, or when the retry budget for invalid calls is
   spent, a fixed fallback line is spoken (item 4).
5. A barge-in cancels the token: ReadOnly calls are dropped, SideEffect calls finish on their own
   task and reach the next turn as late notes, and a regenerated identical call is answered from
   the record (item 8, "Regeneration"). The runner joins the round task before it touches the
   context, then cuts the context to what was played (`docs/ARCHITECTURE.md`, "Played-text
   truncation").
6. **Speculation** is opt-in (`EngineConfig::speculation`, default off; item 7): a
   speculative run calls `ToolEngine::speculate`, which runs ReadOnly calls and **holds**
   SideEffect calls; only the commit at the real end of turn runs them (`ToolEngine::commit`), and
   a dropped speculation never runs them. Starting the LLM speculatively by default is a follow-up
   (it exists as an opt-in speculative LLM start), not part of the base round loop.
7. **Metrics**: one timeline record per request round (next section); the engine's stage times
   (`docs/LATENCY.md`, "Engine stage times and memory") take the first round of a turn; the
   later rounds of a tool loop are not in the benchmark yet.

## Remote tools
An agent process (`docs/AGENT_PROTOCOL.md`) declares tools in its `session.accept`; the link
registers each one as an ordinary tool of the call's registry whose `Tool::call` is an RPC
(`brisvox_agent_link`, `tool.rs`): a `tool.call` message to the agent, its `tool.result` back.
Everything above applies unchanged: schema validation before the call leaves the server, the
retry budget for malformed calls, the idempotency store (the `idempotency_key` in `tool.call` is
the digest of `(session, epoch, call id)`), the result bounds, the filler policy
(`filler_after_ms`), speculation (ReadOnly only).

- **Timeouts**: the declared `timeout_ms` (default 5 s, at most 30 s) is the spec's timeout. A
  ReadOnly call is dropped by the engine at it or at a barge-in; dropping the RPC sends
  `tool.cancel` and a late `tool.result` is ignored.
- **SideEffect** (item 8): the engine answers the slot `timeout` and keeps the future;
  the link keeps waiting up to `side_effect_wait` (60 s, under the 120 s backstop), so a late
  result becomes a late note. After that wait, or when the link goes down, the call ends with
  the message "the outcome is unknown" (a `tool_failed` result or late note): never retried,
  never reported as a plain failure.
- **Bounds**: at most 64 declared tools, 32 calls waiting per link; a full control queue or a
  link that is down fails the call at once (`tool_failed`) instead of waiting.
- A remote tool whose name collides with a tool of the agent definition is refused at the
  handshake and reported to the agent (`error`, stage `agent_link`).
- `tools.set` narrows the offered tools from the next response on; a call to a tool that is not
  offered is answered `{"error": "unknown_tool", ...}` without running (the runner's screen,
  before the engine).

## Per-request LLM metrics (`brisvox_core::timeline`)
The turn timeline timed only the first LLM request. A tool loop issues one request per round.
- `TimelineRecorder::llm_request(epoch, at) -> Option<usize>`: opens the next round (returns its
  index) and marks `TurnEvent::LlmRequest` (first wins, as before).
- `TimelineRecorder::llm_first_token(epoch, round, at)`: the round's first token. Also marks
  `TurnEvent::LlmFirstToken` (first wins).
- `TimelineRecorder::llm_done(epoch, round, at)`: the round's completion.
- `TurnRecord::llm_rounds()`, `llm_round(i)` and `llm_round_count()` give `LlmRoundMetrics
  { round, time_to_first_token, duration }`. Each is derived from its own marks.
- At most `MAX_LLM_ROUNDS` (32) requests are timed per turn. A longer tool loop is beyond the
  metrics: its requests get no record and count in `TimelineStats::llm_rounds_dropped`. This
  deviates from "every request gets its own metrics" only past 32 rounds, which the turn loop should
  stop anyway. Round marks for a finished turn are late, as other marks.
- Counters: each `llm_*` call counts once in `TimelineStats` (`recorded` if it changed the turn mark
  or opened its round, `repeated` if it changed nothing). `llm_rounds_dropped` counts requests past
  the cap.
- Summaries: `brisvox::timeline` emits one `llm request finished` event per timed round.

## In the server binary (`brisvox_server::tools`)

The binary's `EngineRunner::with_tools` takes a `ToolSetup`: every tool the server can offer,
registered once at startup. An agent definition lists the ones it uses (`agent.tools`,
`docs/CONFIG.md`); a listed tool the setup does not provide stops the start with its position
in the file and what would provide it (fail closed: a call without its tools would answer
wrongly). Each call gets its own `ToolEngine` (the idempotency store is per call) over the
agent's tools only.

| source | tools | how |
|---|---|---|
| application | anything | the plug-in point `brisvox_server::tools::AppTools`: `fn register(&self, &mut ToolRegistry, &SharedClock)`, called once. Your own `main` is one line: `brisvox_server::app::run_with_tools(Arc::new(MyTools))` (below). The tools are shared by every call: per-call state comes from `ToolCallCtx` (session, idempotency key) |
| demo (feature `demo-tools`, off) | `lookup_order`, `book_appointment` | canned answers labelled as demo data, so `examples/agents` run end to end; never for callers |
| telephony (feature `call-control`) | `transfer_call`, `send_dtmf`, `end_call` | registered when `BRISVOX_CALL_CONTROL` names a carrier; destinations from `[telephony]` (`docs/TELEPHONY.md` 5); warm transfers, DTMF over Twilio and answering-machine handling |

Application and demo tools share one registry, so a call pays no schema compile for them. The
telephony tools hold per-call state (one transfer per call, the carrier's call id), so a call
that uses them registers them on its own registry, on the inference pool (`Offload::run`,
bounded at 2 s). The example agents name `lookup_order` and `book_appointment` as the
application tools they stand for: a default build refuses them with the hint
above, a build with `demo-tools` runs them.

### Rust tools in your own binary

The stock `brisvox-server` binary has no tools of your own. To add Rust tools you write a small
binary of your own that depends on the `brisvox` crate (one dependency: it re-exports
`brisvox-server` as `brisvox::server`, the tools API as `brisvox::agent::tools` and the clock types
as `brisvox::core`, and forwards the server's features) and hands your `AppTools` to the same
program the stock binary runs; you do not copy its `main`:

```rust
use std::sync::Arc;

fn main() -> std::process::ExitCode {
    brisvox::run_with_tools(Arc::new(MyTools::default()))
}
```

`brisvox::run_with_tools` is `brisvox_server::app::run_with_tools`, and `brisvox::run()`
(`brisvox_server::app::run()`) is the stock program (no tools of your own) and
`run_with_tools(tools)` is the same with `tools` registered next to the built-in ones. Both take
the settings from the environment exactly like `brisvox-server` (`docs/OPERATIONS.md`), keep its
logging, its exit codes and its feature flags, and understand its command line: no arguments
starts the server, `check <agent-file>` validates an agent file offline and knows your tools'
names (`docs/CONFIG.md`, "Checking a file"), `--help`, `--version`. Enable in your `Cargo.toml` the
`brisvox-server` features your agent file needs (`provider-*`, `webrtc`, `smart-turn-ort`,
`call-control`). The stock binary's `mimalloc` global allocator is declared in its `main.rs`, not
in the library: add your own `#[global_allocator]` if you want it.

**From outside this repository.** The brisvox crates are on crates.io. Use Rust 1.99 or newer (the
crates use edition 2024) and depend on them by version; commit your own `Cargo.lock` to keep the
dependency versions you tested:

```toml
[dependencies]
brisvox = { version = "0.1", features = [
    "provider-deepgram", "provider-openai", "provider-cartesia", "smart-turn-ort",
] }
serde_json = "1"
```

(`cargo add brisvox --features provider-deepgram,provider-openai,provider-cartesia` writes the
same. The `brisvox` crate holds no logic of its own: `brisvox::agent::tools` has `Tool` and
`ToolSpec`, `brisvox::core::SharedClock` the clock. Depending on `brisvox-server`, `brisvox-agent`
and `brisvox-core` separately works the same.)

To follow the development branch instead, depend on the git repository
(`{ git = "https://github.com/bilal07karadeniz/brisvox", rev = "<commit>" }`, pinning a commit)
and copy `rust-toolchain.toml` and `Cargo.lock` next to your `Cargo.toml`.

A fully local agent (no API key) needs the features `provider-sherpa-onnx`,
`provider-openai-compatible`, `provider-openai-speech`, `smart-turn-ort` and, for browsers,
`webrtc` (`book/src/quickstart/local.md`). The whole standalone `Cargo.toml` is in
`examples/rust-tools/README.md`.

`examples/rust-tools` is a complete, runnable project of exactly this shape, with its agent file:

- `get_time(timezone)` is a `ReadOnly` tool: it changes nothing, so the engine may run it
  speculatively and cancels it when the caller barges in. It reads time from the injected clock
  (`SharedClock::wall`), and a bad argument is a `ToolError` whose text tells the model what to
  say or retry.
- `save_note(text)` is a `SideEffect` tool: never speculative, never run twice for one call. It
  still keys its effect on `ctx.idempotency_key()`, so a redelivered request gets the same note
  back. Key your own store on the whole `IdempotencyKey` (distinct keys may share a digest); pass
  `digest()` only to an external API as its idempotency key.
- State that outlives a call (here the notes) lives behind an `Arc` inside your `AppTools`; per-call
  data comes only from `ToolCallCtx`.

Run it with `cargo run -p brisvox-example-rust-tools -- check examples/rust-tools/agent.toml`, and
with keys and `BRISVOX_AGENT_FILE=examples/rust-tools/agent.toml` set, without `check`, to serve
calls. The crate's tests register the tools and load the agent file against them, so a renamed tool
or a stale agent file fails the build.

If your tools live in another process or language, use the
Python or TypeScript SDK (`sdk/python`, `sdk/typescript`) instead; the agent file then lists no
`agent.tools` for them (the agent link supplies them).

## Confirmation gate for SideEffect tools

Idempotency stops a SideEffect tool from running *twice*; it does not stop it from running
*wrongly*. A caller (or text the caller reads out, or a document a tool returned) can talk the
model into calling `transfer_call` to a number of their choice. A tool declared with
`confirm = true` is therefore **held**: the model's call does not run it.

**Declaring it.** Rust: `ToolSpec::new(..).with_confirm(true)` (SideEffect tools only; registering
a ReadOnly tool with `confirm` is `RegistryError::ConfirmNeedsSideEffect`). Agent definition
(TOML or YAML): list the tool in `agent.confirm_tools` (a subset of `agent.tools`), which turns
the gate on for tools the server registered without the option, built-in telephony tools
included (`confirm_tools = ["transfer_call"]`). Agent link: `"confirm": true` in the tool
declaration (`docs/AGENT_PROTOCOL.md`). Registering a confirm tool adds the built-in tool
`confirm_pending` to the registry (the name is reserved); a registry with no confirm tool has
none.

**What happens.**

1. The model calls the tool. Arguments are validated as always. The engine does not run it; it
   records a *pending confirmation* and answers with this result (not an error):
   `{"status":"confirmation_required","tool":"<name>","call_id":"t3.0","message":"Not done yet: this action needs the user's confirmation. Tell the user exactly what you are about to do and ask them to confirm. Only if the user clearly agrees in their NEXT reply, call confirm_pending with this call_id. If they decline, change the request or say anything else, do not call it. Never confirm on your own."}`
2. The model asks the user. The user answers in a **later user turn**.
3. If the model judges that the reply clearly agrees, it calls `confirm_pending(call_id)`. The
   engine then runs the held call, with the arguments of the original request (the confirmation
   cannot change them), through the normal SideEffect path: its own idempotency key, timeout,
   backstop and late note. Its result is the result of the `confirm_pending` call.

**What the engine enforces** (the meaning of "yes" is the model's judgment; the *order of events*
is not):

- *A later user turn.* A request and its confirmation can never belong to the same user turn,
  however many tool rounds the model makes inside it. The runner reports each ended user turn
  (`Confirmations::note_user_turn`, called when the user's message enters the context; a
  conversation edit made by an agent is not a user turn); a `confirm_pending` made before one
  has ended since the request is refused ("the user has not replied since you asked") and the
  request stays pending. In a speech-to-speech session the provider's "speech stopped" event can
  arrive after the model already answered and called the tool: the speech that was still going on
  when the request was made does not count as the reply
  (`Confirmations::note_user_speech_started` marks where a speech began), so a request and its
  confirm cannot share a turn even then.
- *Epoch.* A request records the turn epoch it was made in; a confirmation from an older epoch is
  refused as stale.
- *Expiry.* A request lives `EngineConfig::confirm_ttl` (default 120 s) on the injected clock.
- *Barge-in clears.* An interruption of the bot drops every pending request
  (`Confirmations::clear`): the user may not have heard what they would confirm, and the context
  after an interruption holds only what was played. The cost is one more question when a caller
  talks over the question.
- *Once.* Taking a request removes it, so a second `confirm_pending` finds nothing; a replay of
  the same confirming call returns the recorded result (the idempotency store).
- *Bounded.* At most 8 requests wait at once (the oldest is dropped); a repeated call with the
  same tool and arguments replaces its pending request.
- *Per session.* The pending set lives in the call's `ToolEngine`; nothing is shared between
  calls.
- *Speculation.* Like any SideEffect call, a gated call is held by a speculative response and
  recorded only when the response is committed.

**Logging.** Every SideEffect execution is logged (`brisvox::tools`, `info`) with its tool name,
epoch and idempotency key digest, and a confirmation additionally with the id of the request it
confirms. Arguments are never logged (a phone number is personal data, `docs/PRIVACY.md`).

**Known limits.** A confirmation is the model's decision, so a model that is itself fully
controlled by an attacker *and* a caller who really says "yes" still passes: the gate protects
against the model acting without the user's say-so, not against the user. S2S sessions report
user turns by speech end and barge-ins by their own events. A gated call takes no part in the
orphan replay (the orphan window, `TOOLS.md` item 7), and its result is never reported as a late
note: a request whose result did not reach the model is cleared by the barge-in, and when the model
repeats the identical call it records a **fresh** request with a fresh `call_id`, which the user's
"yes" then confirms. `confirm_pending` itself cannot be listed in `confirm_tools` (the name is
reserved).

## Verification: parallel and chained calls

Double and triple tool calls and parallel tool calls get their own verification (2026-10-14). Everything above is
tested piece by piece; `crates/brisvox-engine/tests/tool_scenarios/` tests the pieces together, on the
production path: `LlmRunner` with its `ToolEngine`, filler policy and interruption handling, in the
runner's pipeline (the rig of `tests/common/llm.rs`), over a provider. One scenario is one function
(`scenarios.rs`) that runs against

- **scripted providers**, in the normal suite (`mock.rs`): the simulator's `MockLlm` on a paused
  clock (calls arriving one after the other: `plain`, and with interleaved argument fragments:
  `interleaved`) and the **real adapters** `OpenAiCompatibleLlm`, `AnthropicLlm` and `GeminiLlm`
  over loopback servers (`wire.rs`) that speak each vendor's streaming format for parallel calls,
  in real time. The script says what the model does, so the assertions are exact: wall time of the
  tool phase, concurrency (every call started before the first finished), result order in the
  context and **on the provider's wire** (one `tool` message per result for OpenAI; the
  `tool_result` blocks of one user message for Anthropic; the `functionResponse` parts of one user
  content, with the thought signature echoed, for Gemini), the number of requests (a bug that adds
  a round fails), the runner's counters;
- **real providers** with `BRISVOX_LIVE_TESTS=1` (`live.rs`), with loose assertions that hold
  whatever a real model chooses. The loose assertions are themselves run against the scripts
  (`mock::loose`), so a mistake in them shows up in the suite and not on the first run with keys.

| # | Scenario | Tools | What the exact (scripted) run pins |
|---|---|---|---|
| S1 | three independent lookups in one response | `get_weather` x3, 600/400/200 ms | one tool round, three calls, tool phase 600 ms (the longest, not the sum 1200), results in issue order though Cairo finishes first, one spoken answer, no filler |
| S2 | chain of three rounds | `get_order` -> `get_customer` -> `get_weather` | four requests, three rounds, each tool starts after the previous one finished, each request carries one more result (also on the wire), final answer names the city and temperature |
| S3 | a booking next to a lookup, the model repeats the booking | `book_table` (SideEffect), `get_weather` | the table is booked once (`BK-1001`, never `BK-1002`); the repeat is answered `already_done`; three results in issue order |
| S4 | arguments that break the schema | `get_forecast` (`days` max 3) | the invalid call never reaches the tool; structured `invalid_arguments` with the JSON pointer `/days`; the valid retry runs; one unit of retry budget |
| S4b | the model never gets it right | `get_forecast` | exactly three requests, nothing runs, `budget_exhausted`, one spoken fallback, no endless loop |
| S4c | broken calls among good ones | `get_weather` + an unknown tool | results ok / `invalid_arguments` / `unknown_tool` / ok in issue order, the two good calls run together, the retry budget is spent once |
| S5 | a slow tool | `get_weather` 2 s, filler after 1.2 s | exactly one filler, at the policy's delay (+/- 30 ms of simulated time), before the tool ends, then the answer |
| S5b | three slow calls | `get_weather` x3, 2 to 3 s | still one filler; phase length is the slowest call |
| S5c | a tool past its timeout | `get_weather` 5 s, timeout 600 ms | the future is dropped, `timeout` result, one filler, the model apologises, the next request goes out at the timeout and not at 5 s |
| S6 | barge-in while the tools run | `book_table` 600 ms + `get_weather` 5 s | the ReadOnly call is dropped (never finishes), the booking finishes once, both open calls closed `interrupted`, one late note with `BK-1001` before the caller's next words, and in the next request |

Speech-to-speech (`s2s.rs`, the `S2sStage` with a scripted provider that sends what Gemini Live
sends): S1 (three `ToolCall`s of one response run together; their three results go to the provider in
call order and **one** `CreateResponse` follows), S2 (a three-round chain: one result and one
`CreateResponse` per round), S3 (a repeated booking is answered `already_done`).

### Mock results (normal suite, 2026-10-14)

`cargo nextest run -p brisvox-engine --test tool_scenarios`: 77 tests run without keys and all pass (plus 4 live tests that skip without `BRISVOX_LIVE_TESTS=1`). Each scenario x
shape cell below is one test (`plain` and `interleaved` on `MockLlm`, the three adapters over
loopback); the last two rows are the loose (live) assertions against the scripts and the
speech-to-speech stage.

| Scenario | MockLlm plain | MockLlm interleaved | OpenAI-compatible | Anthropic | Gemini |
|---|---|---|---|---|---|
| S1 parallel | pass | pass | pass | pass | pass |
| S2 chain | pass | pass | pass | pass | pass |
| S3 repeated booking | pass | pass | pass | pass | pass |
| S4 invalid then valid | pass | pass | pass | pass | pass |
| S4b invalid forever | pass | pass | pass | pass | pass |
| S4c broken among good | pass | pass | pass | pass | pass |
| S5 slow tool + filler | pass | pass | pass | pass | pass |
| S5b slow parallel, one filler | pass | pass | pass | pass | pass |
| S5c tool timeout | pass | pass | pass | pass | pass |
| S6 barge-in | pass | pass | pass | pass | pass |
| loose (live) assertions, S1 to S6 | pass | - | pass | pass | pass |
| S2S stage: S1, S2, S3 | scripted Gemini Live events: pass | | | | |

**Bug found and fixed (S3).** Before the fix the model's repeat of `book_table` ran the tool again:
two bookings, `BK-1001` and `BK-1002`. Call ids are unique within a turn, so the idempotency key
(session, epoch, call id) never matched, and the orphan check covers only calls whose result the
model did not read. The engine now refuses an identical SideEffect call of a later response of the
same turn when the first one succeeded (`already_done`, see "Repeats within a turn" above;
`ToolSpec::repeatable` opts out). Regression tests: `crates/brisvox-agent/tests/tools.rs`
(`an_identical_side_effect_call_of_a_later_response_of_the_same_turn_is_refused` and four more for
the edges), mutation-checked (disabling the check, dropping the epoch check and counting failed
calls as successes each fail tests); and S3 above on every shape. Two fixture changes in
existing tests: the store-bound tests of `tools.rs` used one set of arguments for many call ids and
now use distinct ones, and the flow tests' `act` tool is `repeatable`.

### Running the live variants

```text
BRISVOX_LIVE_TESTS=1 cargo test -p brisvox-engine --test tool_scenarios live_ -- --nocapture --test-threads=1
```

One test per provider (`live_openrouter`, `live_anthropic`, `live_gemini`, `live_gemini_live`); a
provider without its key skips with a message. The keys come from the environment (never printed):

| Provider | Variables |
|---|---|
| OpenRouter (OpenAI-compatible adapter) | `OPENAI_COMPATIBLE_API_KEY`; optional `OPENAI_COMPATIBLE_BASE_URL` (default OpenRouter), `OPENAI_COMPATIBLE_MODEL` (default `openai/gpt-6-luna`), `OPENAI_COMPATIBLE_REASONING_EFFORT` (default `none`, empty omits it) |
| Anthropic | `ANTHROPIC_API_KEY`; optional `ANTHROPIC_MODEL` |
| Gemini | `GEMINI_API_KEY` (or `GOOGLE_API_KEY`); optional `GEMINI_MODEL` (default `gemini-3.1-flash-lite`). Free tier: about 5 requests a minute and 20 a day per model, so requests are spaced 13 s, a 429 skips the scenario with a message, and only S1 to S3 run by default |
| Ollama (OpenAI-compatible adapter, `Preset::Ollama`, no key) | `OLLAMA_MODEL` (required, one model per run; the test skips without it); optional `OLLAMA_BASE_URL` (default `http://127.0.0.1:11434/v1`), `OLLAMA_REASONING_EFFORT` (default `none`, empty omits the field), `OLLAMA_MAX_TOKENS` (default 600), `OLLAMA_SCENARIOS`. The test is `live_ollama`; the latency test is `ollama_voice_latency` in `crates/brisvox-providers/tests/openai_compatible/live.rs` (`OLLAMA_RUNS`, default 5) |
| Gemini Live (S2S stage) | the Gemini key; optional `GEMINI_LIVE_MODEL` (default `gemini-3.8-live`); S1 and S2 only |

`BRISVOX_LIVE_SCENARIOS=1,2,6` picks scenarios for every provider; `OPENROUTER_SCENARIOS`,
`ANTHROPIC_SCENARIOS`, `GEMINI_SCENARIOS` and `GEMINI_LIVE_SCENARIOS` override per provider. A run
prints, per scenario, every round (time to first event and to first text, total, calls, text bytes),
every tool (start, finish, dropped), the runner's counters and what was spoken, then
`LIVE RESULT <provider> scenario <n>: ok|FAILED (<seconds> s)`. The live scenarios 5 and 6 use real
time (1 s tool, 0.4 s filler; 0.6 s booking). Gemini Live drives the engine's S2S stage with a
recorded sentence of the FLEURS fixtures ("The East African Islands are in the Indian Ocean off the
eastern coast of Africa."): the prompt turns it into three independent lookups (S1) or a
country -> capital -> weather chain (S2).

### Live results

Run 2026-10-14 (`BRISVOX_LIVE_TESTS=1 cargo test -p brisvox-engine --all-features --test
tool_scenarios live_ -- --nocapture --test-threads=1`; Gemini text requests spaced 13 s for the
free tier). Cells: result, the issuing round's time to first event, rounds.

| Scenario | OpenRouter `openai/gpt-6-luna` | Anthropic (Haiku 5.5) | Gemini `gemini-3.1-flash-lite` | Gemini Live `gemini-3.8-live` |
|---|---|---|---|---|
| S1 parallel (3 calls in one response) | ok: 3 calls, TTFT 2.0 s, 1 tool round, total 4.0 s | ok: 3 calls, 1.4 s, 1 round, 3.3 s | ok: 3 calls, 1.8 s, 1 round | ok: 3 calls run together (307 ms phase), spoken answer |
| S2 chain (3 rounds) | ok: 3 rounds, 1.1-1.2 s per round, 6.1 s | ok: 3 rounds, 0.6-1.1 s per round, 5.1 s | ok: 3 rounds (spaced 13 s) | ok: Africa -> Algeria -> Algiers -> weather, 3 rounds |
| S3 booking repeated | ok: booked once | ok: booked once | ok | n/a |
| S4 invalid args | ok: retried with valid args | ok | n/a | n/a |
| S5 slow tool + filler | ok: 1 filler | ok: 1 filler | n/a | n/a |
| S6 barge-in | ok: late note next turn | ok: late note next turn | n/a | n/a |

Gemini Live first failed every connect: the adapter sent the raw JSON Schema in `parameters`,
which takes only Gemini's OpenAPI subset and refuses the setup on `additionalProperties`; it now
sends `parametersJsonSchema` (fixed with this run).

### Local models through Ollama

Run 2026-10-14 against Ollama 0.34.1 on Windows, an RTX 5070 (12 GB), through the same engine path
as the table above (`LlmRunner` + `ToolEngine`, the OpenAI-compatible adapter with
`preset = "ollama"`, `reasoning_effort = "none"`, sampling left at each model's Ollama defaults, the
server's default context of 4096 tokens). One model at a time:

```text
OLLAMA_MODEL=gemma4:12b-it-qat BRISVOX_LIVE_TESTS=1 cargo test -p brisvox-engine --all-features   --test tool_scenarios live_ollama -- --nocapture --test-threads=1
OLLAMA_MODEL=gemma4:12b-it-qat BRISVOX_LIVE_TESTS=1 cargo test -p brisvox-providers --all-features   --test openai_compatible ollama_voice_latency -- --nocapture
```

Each model ran the six scenarios three times (18 runs). Latency: a one-to-two sentence voice
prompt on a warm model, five runs, p50 of the time to the first text token and to the end of the
first sentence. The GPU had room for every model (no other large GPU process was resident), so
"GPU memory" is the full footprint that `/api/ps` reports at the 4096 context.

| Model | Params, quant | GPU memory | Scenarios passed (of 18) | TTFT p50 | First sentence p50 |
|---|---|---|---|---|---|
| `gemma4:12b-it-qat` | 11.9B, Q4_0 | 7.7 GB (all on GPU) | **18/18**: S1 to S6 each 3/3 | 69 ms | 267 ms |
| `gemma4:e4b-it-qat` | 7.5B (4B effective), Q4_0 | 3.1 GB | 12/18: S2 0/3, S4 0/3, rest 3/3 | 42 ms | 157 ms |
| `qwen3.5:4b` | 4.7B, Q4_K_M | 3.1 GB | 12/18: S2 0/3, S4 0/3, rest 3/3 | 65 ms | 208 ms |
| `qwen3.5:9b` | 9.7B, Q4_K_M | 5.5 GB | 12/18: S1 2/3, S2 2/3, S4 0/3, S5 2/3, S3 and S6 3/3 | 79 ms | 291 ms |
| `qwen2.5-coder:7b` | 7.6B, Q4_K_M | 4.7 GB | 0/18 | 34 ms | 220 ms |

How the others fail (the failures are the models', not the engine's; the engine refused every bad
call and ran every good one):

- **S2, a chain whose second call needs the first result.** `qwen3.5:4b` and, once in three,
  `qwen3.5:9b` call `get_customer` and `get_weather` in the same response with a guessed city
  (Rome, Paris) instead of waiting for `get_customer`. `gemma4:e4b-it-qat` answered nothing at all
  (an empty response, no text and no call).
- **S4, an invalid argument (`days` above its maximum).** Every model but `gemma4:12b-it-qat` read
  the structured `invalid_arguments` result, said "I will ask for three days instead" and ended the
  turn without calling the tool again. The caller hears a promise that is never kept.
- **S1 and S5.** `qwen3.5:9b` once answered the three-city question and once the slow Izmir
  question with made-up weather, without calling any tool (a hallucinated answer, the worst
  failure for a voice agent).
- **`qwen2.5-coder:7b`** never emits a native tool call: it writes the call as JSON text
  (`{"name": "get_weather", "arguments": ...}`), which the agent would speak aloud. S6 also waited
  its full 120 s for tools that never started. Do not use it for tools.

Thinking. `qwen3.5` and `gemma4` are thinking models that think by default on Ollama.
`reasoning_effort = "none"` on the OpenAI-compatible endpoint turns that off and is what the table
used (a model without a thinking mode, `qwen2.5-coder`, accepts the field). The native field
`think: false` is ignored on `/v1/chat/completions` (`qwen3.5:4b` spent 300 of 300 tokens on
reasoning and returned an empty answer). Without the setting, `qwen3.5:4b` produced no spoken text
inside a 200-token budget. With `reasoning_effort = "medium"`, `qwen3.5:9b` passed 12/12 scenario
runs, but the plain chat prompts took 14 to 35 s to the first spoken word (TTFT p50 22 s), and
`qwen3.5:4b` still failed S2: thinking buys tool reliability at a price a call cannot pay.

Partial offload. `gemma4:12b-it-qat` needs 7.7 GB. With only 5 GB of GPU memory free (the Kokoro
voice server resident next to it), 30 of its 48 layers on the GPU (4.96 GB) gave TTFT p50 376 ms
and first sentence p50 1.66 s on the same prompts (measured through Ollama's native
`/api/chat` with `num_gpu = 30` and `think = false`, because `/v1` has no per-request options;
the model is the same, the adapter adds a few ms). That is usable for a call but no longer
fast: either give the language model the GPU alone (Kokoro's CPU image), or accept the 1.7 s.
The first request after a model was idle paid its load: 7 to 50 s (cold) in these runs, so warm
the model before a call (`OLLAMA_KEEP_ALIVE=-1` keeps it resident).

Recommendation: `gemma4:12b-it-qat` for any agent with tools, `reasoning_effort = "none"` always.
Without tools, `gemma4:e4b-it-qat` is the fastest (157 ms to the first sentence in 3.1 GB). The
example agents in `examples/agents/local/` use the first. Three runs per model is a small sample:
a model that passed 3/3 here can still fail on a prompt the scenarios do not cover, so try your own
tools.
