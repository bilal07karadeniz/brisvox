# Live V2V with 5 concurrent calls: stopped by Cartesia's concurrency limit (2026-10-14, Windows)

The planned run (100 turns per system, blocks of 5 concurrent calls x 2 turns, brisvox and Pipecat
alternating) was **stopped after the first warm-up block of each system**: the Cartesia account's plan
allows **2 concurrent TTS generations** (`You have reached your concurrency limit of 2, please upgrade
your subscription for a higher concurrency limit.`), and five calls that speak at the same moment
exceed it. Following the run's rule (a provider that refuses concurrency ends the run, no
aggressive retries), nothing else was run: 2 warm-up blocks, 20 turns, about USD 0.002 of OpenRouter
credit. The run was then done with Deepgram Aura as the TTS for both systems: `v2v-live-{brisvox,pipecat}-c5-windows-2026-10-14.{md,json}` (this note is the record of the Cartesia attempt).

Setup of the two blocks (identical for both systems): `brisvox-bench v2v --url ws://127.0.0.1:PORT/
--fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000` against the system already
running on `bench/pipecat_baseline/agent_live_en.toml` (Deepgram `nova-3`, `openai/gpt-6-luna` on
OpenRouter with `reasoning_effort` none and the upstream pinned to `openai` without fallbacks,
Cartesia `sonic-3.6`). The 5 calls play the same fixture, so their turns end within about a
second of each other and their replies are synthesised at nearly the same time.

| | brisvox | Pipecat 1.12.0 |
|---|---|---|
| calls / turns | 5 / 10 | 5 / 10 |
| answered | 10 | 7 (1 answered only after the user spoke again, 2 `no_response`: one whole call got no audio) |
| V2V of the answered turns (ms), sorted | 1485 1725 2065 2256 2282 2282 2869 3539 3603 4720 (p50 2282) | 1899 1900 2201 2205 2316 2562 2626 (and 14386 for the turn answered late) |
| Cartesia refusals | handled by the adapter: a `concurrency_limited` refusal of a context that produced nothing is re-opened after 300 ms, up to 4 times (`brisvox_providers::cartesia` module docs, `concurrency_retry_delay`); `tts_ttfb` of the turns: 215 233 234 215 228 691 694 1466 1471 2563 ms | 12 `concurrency limit of 2` error frames (`CartesiaTTSService`), the affected contexts produced no audio |

So at 5 concurrent calls on this Cartesia plan the two systems are no longer doing the same work:
brisvox waits and retries (its V2V grows by the retry delays, up to +2.3 s on a turn), Pipecat as
shipped drops the reply. A latency comparison at this concurrency would measure Cartesia's
quota handling, not the frameworks. The comparison needs either a Cartesia plan with a concurrency
of at least 5 (every call can speak at the same moment), or at most 2 concurrent calls on the
current plan (`brisvox-bench v2v --concurrency 2`).

Provider routing verification of these blocks (the 20 turns): all 10 brisvox requests logged
`llm upstream ... upstream_provider="OpenAI"`; of Pipecat's 14 LLM requests 10 returned a stream and
all 10 logged `PCUPSTREAM ... provider=OpenAI` (the other 4 produced no chunk, so there is no
provider to report: Pipecat's turn restarts cancel a request when the user resumes). Negative control with the pinned slug replaced by a name that does
not exist (`openrouter_order = ["nonexistent-provider-xyz"]`, one call per system, nothing was
answered): both systems got OpenRouter's HTTP 404 (`No endpoints found for openai/gpt-6-luna`) on
every LLM request, proving that both really send `provider.order` / `allow_fallbacks: false` and that
a pin that cannot be met fails instead of falling back.

Model route used: `openai/gpt-6-luna` (resolved endpoint list on 2026-10-14: `openai`,
`openai/fast`, `openai/flex`, `azure*`, `amazon-bedrock`); effort `none`, the lowest this model
accepts (`supported_efforts`: max, xhigh, high, medium, low, none; there is no `minimal`); pinned
with `{"provider": {"order": ["openai"], "allow_fallbacks": false}}`.
