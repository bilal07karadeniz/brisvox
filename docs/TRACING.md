# Errors and tracing conventions

Status: in force. Code: `brisvox_core::{error, telemetry}`,
`brisvox_server::telemetry`.

## Errors
- Every library crate has `src/error.rs` with one `#[non_exhaustive] pub enum Error` built with
  `thiserror`, plus `pub type Result<T, E = Error>`. Both are re-exported at the crate root.
- `brisvox_core::Error` holds the cross-cutting cases (`Cancelled`, `Timeout { what, after }`,
  `ChannelClosed`). Every other crate wraps it as `Error::Core(#[from] brisvox_core::Error)`
  (`#[error(transparent)]`), so `?` converts it without extra code.
- No `anyhow` / `eyre` in library crates. Binaries report errors with `eprintln!` + `ExitCode`.
- Error messages never contain credentials. Provider error `detail` strings are written by our
  adapters and must not echo request headers or URLs that carry keys.
- `Cancelled` is not a fault: log it at DEBUG at most, never at ERROR.

## Spans
Create spans only through the constructors in `brisvox_core::telemetry`, so names and fields
stay stable for log queries:

| Span | Constructor | Level | Fields | Lifetime |
|---|---|---|---|---|
| `session` | `session_span(id)` | INFO | `id` | whole session (root of all session work) |
| `turn` | `turn_span(epoch)` | INFO | `epoch` | one turn; the epoch is the interrupt epoch |
| `stage` | `stage_span(name)` | DEBUG | `name` (`"vad"`, `"stt"`, `"llm"`, `"tts"`, …) | one stage's work inside a turn |

Nesting is `session{id}:turn{epoch}:stage{name}`. Spawned tasks must carry their parent span
(`.instrument(span)`), or their events lose the session context.

## Levels
- ERROR: a session failed or a server-level fault happened. Someone should look.
- WARN: degraded but recovered (retry, fallback provider, dropped frames by overflow policy).
- INFO: lifecycle (session start/end, turn boundaries, provider connect/close). Low volume only.
- DEBUG: per-stage detail, decisions (turn-end, interruption reasons).
- TRACE: per-frame / per-token detail. Never emit above TRACE on the per-frame hot path, and
  never create spans per audio frame. Disabled callsites are near-free (cached interest check).

## Secrets
- Every credential (API keys, tokens, auth headers, signed URLs) is stored as
  `brisvox_core::Secret<T>`. Its `Debug` and `Display` print `[REDACTED]`, so logging a config
  struct with `?cfg` is safe.
- Read the value only with `Secret::expose()` where it is sent to its destination. Grep for
  `expose()` when auditing.
- `Secret` has no `PartialEq`/`Deref`/`AsRef`/`Hash`: `==` on credentials is not constant-time,
  and implicit access would bypass `expose()`. When serde arrives: implement `Deserialize` only;
  any `Serialize` must write `[REDACTED]`.
- Test `telemetry::tests::secret_is_redacted_in_tracing_output` proves secrets do not leak
  through `%`, `?`, nested `Debug` and format arguments.

## Subscriber
Binaries call `brisvox_server::telemetry::init(format)` once at startup:
- Filter from `RUST_LOG` (default `info`; unset or blank both mean the default). An invalid or
  non-unicode `RUST_LOG` is a startup error, not a silent fallback.
- The TURN client crates' events and spans (`turn_client_proto`, `stun_proto`, `turn_types`) below
  `warn` are dropped by a per-layer filter in front of the output, whatever `RUST_LOG` says
  (they print whole STUN messages at `trace`); `BRISVOX_LOG_TURN_CLIENT=1` lifts it. Embedders
  with their own subscriber apply `brisvox_transport::webrtc::turn_client_log_allowed` (or
  `brisvox_core::telemetry::turn_client_log_allowed`) themselves.
- The SIP stack's events and spans (`rsipstack`) below `warn` are held back the same way, whatever
  `RUST_LOG` says: the stack logs every SIP message at `info`, whole, which on a secure call includes
  the SDES SRTP master keys. `BRISVOX_LOG_SIP_STACK=1` lifts it (debugging only); embedders
  apply `brisvox_core::telemetry::sip_stack_log_allowed`.
- Format from `BRISVOX_LOG_FORMAT`: `text` (default) or `json` (one object per line).
- A second `init`/`install` returns `Error::Telemetry` and does not panic. `install(format,
  filter)` is the environment-free variant (used by tests and embedders).

Libraries never install a subscriber. Tests that need log output install a scoped one with
`tracing::subscriber::with_default`.

## Metrics, traces and log redaction
`brisvox_server::observability` (operator view: `docs/OPERATIONS.md`, "Observability"):

- **Turn traces are built from the timeline, not from `tracing` spans.** The per-stage marks
  (`brisvox_core::timeline`) already hold every stage's start and end on the call's
  `Clock`. When a turn finishes, the `TimelineSink` of the call's recorder turns its
  `TurnRecord` into one OpenTelemetry trace (a `turn` root and a child span per stage) and the
  stage latencies into Prometheus histograms. The `session`/`turn`/`stage` spans above stay what
  they are: log context. Nothing is added per audio frame, token or chunk.
- **Cost.** One finished turn costs a handful of atomics, about a dozen small allocations for the
  trace and one non-blocking `export`; `observability::tests::a_turn_costs_microseconds` measures
  the whole observed turn, 11 marks included.
- **Exporters** implement `TraceExporter` and must not block. `MemoryExporter` (bounded) is for
  tests and embedders; `OtlpExporter` (feature `otlp`) queues to a bounded channel (drops the
  newest when full, counted) and posts OTLP/HTTP JSON in batches under a timeout.
- **Redaction.** `telemetry::init` installs the subscriber with a `RedactingMakeWriter`: each
  event is buffered whole and scrubbed (`Redactor`) before it is written, so a credential in
  an error text, a URL or a header dump never reaches stdout. `Secret` remains the first line;
  `observability::tests::secrets_never_appear_in_{text,json}_logs` run the real subscriber.
  A new place that writes logs by another route (a custom writer) must use the same wrapper.
  With `BRISVOX_LOG_REDACT_PII=true` the same pass also replaces personal identifiers (emails,
  IBANs, IDs, card and phone numbers; `Redactor::with_pii`, `docs/PRIVACY.md` "Logs").
