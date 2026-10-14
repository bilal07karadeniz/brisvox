# Risk register (each risk → mitigation → proving test)

| # | Risk | Mitigation | Proof |
|---|---|---|---|
| R1 | Cross-session state mix-up in inference | per-session state, no batching | inference pool and VAD tests |
| R2 | CPU work stalls tokio → latency for all | dedicated pool; performance review | inference pool tests, in-process load simulation |
| R3 | Provider concurrency limits cap capacity before our code | per-provider semaphores, admission | admission and budget tests, capacity benchmark |
| R4 | One crash drops all calls | per-session supervision, panic isolation, drain | session manager isolation and drain tests |
| R5 | False barge-in from echo/backchannels | echo guard, min words, backchannel lists | interruption policy tests |
| R6 | Speculative run triggers side effects | hold SideEffect until commit | speculation tests |
| R7 | Context contains unplayed text after interrupt | word-timestamp truncation | played-text truncation tests |
| R8 | Stale audio after interrupt (transport buffers) | epochs + transport clear/mark | turn epoch tests, telephony mark tests |
| R9 | Context growth in long calls | windowing/summarization | long-call context tests |
| R10 | Turkish quality (turn model, STT/TTS, normalization) | verify support, Turkish fixtures | Smart Turn tests, Turkish quality pass |
| R11 | API facts from memory are wrong/outdated | facts fetched from current provider documentation and cited in `docs/providers/` | all provider adapters |
| R12 | Benchmark unfairness vs Pipecat | identical providers/settings, published scripts | Pipecat baseline bot |
| R13 | Privacy/compliance of recordings | opt-in recording, redaction, retention | recording and privacy tests |
| R14 | SDK hop adds latency | out-of-process but off audio path; measured | agent protocol and SDK benchmarks |
| R15 | Twilio client lead replaces `lead_margin` (reasoned, not measured): a small reported lead can keep one word the caller did not hear; a genuine end-mark echo racing a barge-in is read as a cut mark and can drop up to one lead of heard text | `max(lead, lead_margin)` is the fallback rule; stale `Mark` after `Clear` to be stopped in the transport stage | live Twilio measurement (needs keys) |
| R16 | One shared HTTP client per origin multiplexes hundreds of calls onto few HTTP/2 connections; the server's `MAX_CONCURRENT_STREAMS` (often 100) queues call N+1 on the LLM critical path | measure with N concurrent calls against each provider; shard the pool key (`net::http` Key) by a connection index if it limits; the stream-multiplexing mechanics are tested (`tests/gemini/review.rs`) | pre-warming follow-up (capacity/live) |
| R17 | A provider closes an idle TTS socket (Cartesia: 5 min) and the next turn pays a connect | reconnect at `LlmResponseStarted` overlaps the LLM's first token (tested); every call's TTS is a `ResilientTts` that replaces a session idle for 80 % of `idle_limit()` (Cartesia: 4 of 5 minutes) in the background, swapped in only while idle; the server wraps every call's providers (`ResilientProviders`) | `hybrid_prewarm.rs`, `resilience.rs`, `engine_failover.rs`, `brisvox-server/tests/resilience.rs` (a hosted call refreshes its idle session) |
| R18 | A provider outage or a flaky provider fails every call, and retries multiply the load on it | per-provider circuit breakers shared by all calls, retry budgets per call and per process, mid-call recovery with replay, wired into the server for every call; a secondary per stage is opt-in with `[stt|llm|tts].secondary` in the agent definition (`docs/CONFIG.md`) | `engine_failover.rs`, `brisvox-server/tests/resilience.rs` |
| R19 | The int8 Smart Turn model answers differently per OS and CPU instruction set (up to 0.66 off with flipped verdicts on an AVX2-only CPU without VNNI, emulated; 0.007 and 0.096 on 2 of 12 cases on Linux with VNNI), so end-of-turn decisions on hosts that differ from the reference machine may differ | measured and documented (docs/providers/smart-turn.md "Platform dependence"); tests pin only what is machine-independent (features bit-exact, code paths equal on one machine) and compare with the reference only on its kernels; on real VNNI-less hardware not yet measured | `brisvox-inference::smart_turn`, `brisvox-engine::smart_turn_executor` |

Add rows as risks are discovered; link the test that proves the mitigation.

## Inference capacity (pool measured 2026-10-08; model cost assumed)
Per-window model VAD at ~1 ms per 32 ms window (assumed; measured below) costs ~3% of a core
per session: a 4-thread pool at a safe 70% load carries ~90 sessions (the pool's queueing cliff
is sharp just above 100%, measured). "Hundreds of calls per container" needs cross-session batching, an energy
pre-gate (run the model only near speech), or more cores. Mitigation decided with the VAD and Smart Turn work.

Update (measured): Silero VAD v6.2.3 costs ~76 µs per 32 ms window with onnxruntime on
one thread (Python reference, this desktop), not the assumed ~1 ms: ~0.24% of a core per
session, so per-window VAD is not the capacity limit (a 4-thread pool at 70% would carry
~1000 sessions). Re-measure with the Rust runtime and on the target container.

Update (measured): Smart Turn costs ~30 ms of one core per candidate end of turn (not
per window). At one candidate every ~5 s of user speech, that is ~0.6% of a core per talking
session: 300 sessions need ~2 cores of pool capacity at worst-case alignment. Memory: 512 KB
per session (ring + snapshot) plus ~3.4 MB per pool thread and one ONNX Runtime session per
concurrent analysis (300 sessions: ~150 MB + threads x ~3.4 MB). Larger risk: its
int8 output depends on exact features and on the onnxruntime version (up to 0.16 between 1.28
and 1.30), so upgrading `ort` silently changes turn-taking. Mitigation: bit-exact features and
goldens pinned to the bundled runtime; an `ort` upgrade must regenerate the goldens and re-check
verdicts (docs/providers/smart-turn.md).

Update (measured): Smart Turn with 2 intra-op threads is ~17 ms per call at ~34 ms of CPU
(0.7% of a core per talking session, ~2 cores for 300). The opt-in early analysis (a hint at the
first VAD fall) replaces the analysis at the stop when the fall becomes a stop (no extra job) and
costs one extra job per fall that does not: 0 per 6 stops on the fixtures at the default 64 ms
delay, 1 per 6 at 0-32 ms. `early_analysis.max_per_turn` (default 4) bounds it per turn and the
driver counts `early_wasted` / `early_failed` / `early_capped` to watch it (docs/LATENCY.md, "Early analysis").
