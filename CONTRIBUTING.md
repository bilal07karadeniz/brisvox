# Contributing to Brisvox

Brisvox is a Rust-native, open-source real-time voice agent engine. The goals are low
voice-to-voice latency with the same providers a Python framework would use, hundreds of
concurrent calls per container, and correctness (interruptions, tool calls) that does not
silently break. This file lists the engineering rules every change has to respect and how to
check a change before sending it.

Where things are explained: `docs/ARCHITECTURE.md` (the crates and the call graph),
`docs/PIPELINE.md` (the production pipeline), `docs/LATENCY.md` (the budget and the
measurements), `docs/TOOLS.md` (tool calling), `docs/TESTING.md` (the test strategy) and
`docs/RISKS.md`.

## Checking a change

`scripts/verify.sh` is the quality gate. A change is ready when it is green:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo doc --workspace --no-deps --all-features` with `RUSTDOCFLAGS="-D warnings"` (broken or
  redundant intra-doc links fail)
- the tests (`cargo nextest run --workspace --all-features`)
- `cargo deny check`, when `cargo-deny` is installed (`deny.toml`)
- the tests of the SDKs, the examples and the scripts that the change touches (the script lists
  the toolchains they need: Python 3.11+, Node 20+)

`--all-features` builds the WebRTC transport, which compiles C code (libopus and the DTLS
crypto): it needs a C compiler and cmake. The default build needs neither. `docs/TESTING.md`
("Toolchain for `--all-features`") has the details.

Live tests against real providers are off by default. They run only with
`BRISVOX_LIVE_TESTS=1` and the keys in a `.env` file (copy `.env.example`; `.env` is git-ignored
and must never be committed).

## Engineering rules

Violations of these are blocking in review.

- **No blocking or CPU-heavy work on tokio worker threads.** Inference and DSP heavier than
  about 50 microseconds go to the dedicated inference pool (`brisvox-inference`).
- **Every channel is bounded**, with a documented overflow policy. No unbounded growth.
- **Library code does not panic.** No `unwrap`, `expect`, `panic!`, `todo!`, `unreachable!` or
  indexing that can panic. The workspace denies these as clippy lints
  (`[workspace.lints.clippy]` in `Cargo.toml`); tests are exempted through `clippy.toml`
  (`allow-*-in-tests`), so tests may use them. `todo!` and `unimplemented!` stay denied
  everywhere. `unsafe` code is denied and every public item needs documentation.
- **Every network or IO await has an explicit timeout.** Every task is cancellable through a
  `CancellationToken`, and cancellation leaves state consistent.
- **Interruptions use turn epochs.** Frames from a stale epoch are dropped everywhere.
- **After an interruption the LLM context contains only what was actually played.**
- **SideEffect tools never run speculatively and never run twice** (idempotency key).
- **Time comes from the injected `Clock`**, never from `Instant::now()` directly in logic, so
  simulations are deterministic. `clippy.toml` bans the direct calls.
- **Hot path: no per-frame heap allocation.** Use `bytes::Bytes` and reuse buffers. Allocation
  guards in the tests (`tests/*_alloc.rs`) pin this.
- **Sessions are isolated.** One session's panic or error must not affect others.

## Tests

- Prefer deterministic tests on the simulated clock (`SimClock`, paused tokio time) over
  wall-clock sleeps. A suite that cannot fail proves nothing: the shared simulator suite has
  deliberately faulty pipelines that it must reject.
- Provider adapters are tested against a local mock of the provider's wire protocol, with the
  shared provider contract suite on top. Real-provider checks are the opt-in live tests above.
- A bug fix comes with a test that fails without the fix.

## Dependencies and external facts

- Check crates.io for the current version before adding a dependency, and prefer maintained
  crates under MIT, Apache-2.0 or BSD licenses (`deny.toml` enforces the license list; an
  exception needs a comment saying why). Declare versions once in the workspace
  `Cargo.toml`; crates opt in with `workspace = true`.
- Facts about an external API (message formats, endpoints, limits, model specs) come from its
  current documentation, not from memory. Record them in `docs/providers/<name>.md` with the
  source URL and the date they were fetched.

## Commits and pull requests

- Keep commits small and focused. Write the subject as a plain imperative sentence, for example
  `Drop stale frames in the TTS stage`. Explain the why in the body when it is not obvious.
- Do not commit secrets, keys, personal data or generated build output.
- Keep the documentation in `docs/` in step with the code, and update `docs/LATENCY.md` when a
  change moves a measured number.
