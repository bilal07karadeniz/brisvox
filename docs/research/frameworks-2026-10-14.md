# Competitor facts: LiveKit Agents vs Pipecat (fetched 2026-10-14)

Purpose: fact base for the README comparison table (brisvox vs LiveKit Agents vs Pipecat).
Rule: only what the cited source says. "INFERRED" = my reading, not stated by a source.
"NOT STATED" = the official page was fetched and does not say it.

## 1. Comparison table

| Fact | LiveKit Agents | Pipecat |
|---|---|---|
| Latest version (2026-10-14) | Python `livekit-agents` 1.8.7, released 2026-10-14 [L1][L2]. Node `@livekit/agents` 1.9.2, 2026-10-09 [L3] | `pipecat-ai` 1.12.0, released 2026-09-26 [P1][P2] |
| Release cadence (from release lists) | 1.8.0 on 2026-09-05 to 1.8.7 on 2026-10-14: 8 releases in about 5.5 weeks [L1] | 1.9.0 on 2026-09-11, 1.10.0 09-12, 1.11.0 09-18, 1.12.0 09-26 [P1] |
| Language / runtime | Python library (GitHub primary language Python) [L4]; separate Node.js/TypeScript repo `livekit/agents-js` [L5][L6]. Python `>=3.10,<3.15` [L2] | Python (GitHub primary language Python) [P3]; Python `>=3.11`, `>=3.12` recommended for development [P4][P2] |
| Runtime model | One OS process per job; "each agent server can run multiple jobs simultaneously, running each in its own process for isolation" [L7] | "A single session ties up a process (often a whole container or VM) for the entire duration of a conversation" [P5]. Pipecat Cloud: each instance handles one session at a time [P6] |
| License | Apache-2.0 (framework). Turn-detector text model and `v1-mini` use the separate "LiveKit Model License" [L6][L8] | BSD-2-Clause [P3][P2] |
| GitHub stars (2026-10-14, GitHub API) | `livekit/agents` 14,731; `livekit/agents-js` 942; `livekit/livekit` (server, Go) 21,435; `livekit/sip` 482 [L4][L5][L9][L10] | `pipecat-ai/pipecat` 16,444 [P3] |
| Transport architecture | Agent joins a LiveKit room as a participant. Requires a LiveKit server (Cloud or self-hosted); env `LIVEKIT_URL`, `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET` [L6]. Agent server makes outbound WebSocket connections to the LiveKit server, no inbound public ports needed [L11] | Pluggable transports: Daily, FastAPI WebSocket, LiveKit, SmallWebRTC, Vonage, WebSocket server, WhatsApp, Local, Media over QUIC [P4][P7]. SmallWebRTC is peer-to-peer "without cloud infrastructure" [P7]. Daily/LiveKit/Vonage transports use those vendors' infrastructure [P7] |
| Telephony | LiveKit SIP service. Tested trunks: Twilio, Telnyx, Exotel, Plivo, Wavix, Sinch, didlogic. Inbound/outbound, DTMF (RFC 2833/4733), cold transfer (REFER), warm transfer, SRTP, TLS. Not supported: SIP REGISTER, SIPREC, video over SIP. Self-hosting needs the SIP service deployed separately [L12] | WebSocket + provider `FrameSerializer`: Twilio, Telnyx, Plivo, Exotel (docs); README also lists Genesys and Vonage [P7][P4]. No SIP stack of its own was found in the pages read (INFERRED: SIP via Daily or the provider) |
| Turn detection | Default is the LiveKit turn-detector model. Modes: turn detector, `realtime_llm`, `vad`, `stt`, `manual` [L13]. Audio detector `v1` (LiveKit Inference) and `v1-mini` (local CPU); text detector `MultilingualModel` (Qwen2.5-0.5B-Instruct based) [L8] | Smart Turn v3 (ONNX, bundled, local `LocalSmartTurnAnalyzerV3`) is the default user-turn-stop strategy since v0.0.102 [P8]. Krisp VIVA turn also available [P9] |
| Turkish in turn detection | Yes, both audio detector (14 languages) and text detector (14 languages) list Turkish; text detector Turkish: 99.3% true positive, 87.30% true negative [L8] | Yes: Smart Turn v3.2 supports 23 languages, Turkish listed [P10]. (Pipecat docs page says 23 languages but does not list them [P8]; the model repo README lists Turkish [P10]) |
| Turn model size / CPU cost | Text detector: ~50-160 ms per turn on CPU, 396 MB on disk, <500 MB RAM. `v1-mini`: NOT STATED [L8] | 8 MB int8 (CPU), 32 MB fp32 (GPU). "As little as 10ms on some CPUs, under 100ms on most cloud instances"; ~65 ms on a Pipecat Cloud 1x instance [P10] |
| Turn model license | LiveKit Model License (text detector, `v1-mini`) [L8] | BSD-2-Clause [P10] |
| VAD | Silero plugin; defaults `min_silence_duration` 0.55 s, `activation_threshold` 0.5, 8 kHz or 16 kHz, CPU [L14]. Audio turn detector requires VAD `min_silence_duration` >= 0.25 s [L8] | `SileroVADAnalyzer`; defaults `confidence` 0.7, `start_secs` 0.2, `stop_secs` 0.2, `min_volume` 0.6; 8 kHz or 16 kHz; ONNX, CPU [P11]. Smart Turn requires VAD [P8] |
| Interruption handling | Modes `adaptive` (default on LiveKit Cloud with most STT; separates true interruptions from backchannel) and `vad`. Filters `min_duration`, `min_words`. `false_interruption_timeout` 2.0 s with default resume after a false interruption [L13] | `InterruptionFrame` (SystemFrame) cancels LLM stream and TTS, flushes unplayed output audio; only text actually played is committed to the assistant context. Strategies: `VADUserTurnStartStrategy`, `MinWordsUserTurnStartStrategy(min_words=3)`, Krisp VIVA interruption prediction (backchannel filter) [P12] |
| Tool calling | Function tools; async (background) tools; handoff to another agent; MCP (Python only); provider-side tools (OpenAI, Gemini, Mistral, Anthropic, xAI); toolsets; RPC to frontend. Parallel tool calls: NOT STATED on the tools page [L15] | `run_in_parallel` default True; `group_parallel_tools` default True (LLM triggered once after the batch); `cancel_on_interruption` default True; per-tool `timeout_secs`; async/long-running with `FunctionCallResultProperties(is_final=False)`; `cancellable_by_llm`; `LLMSetToolsFrame`. MCP: NOT STATED on the function-calling page [P13] |
| Speech-to-speech | Realtime models: Amazon Nova Sonic, Azure OpenAI Realtime, Gemini Live, NVIDIA PersonaPlex, OpenAI GPT-Live, OpenAI Realtime, Phonic, xAI Grok Voice Agent, Ultravox (some Python only) [L16] | AWS Nova Sonic, Azure Voice Live, Gemini Multimodal Live, Grok Voice Agent, OpenAI Realtime, Ultravox [P4] |
| Official sizing guidance | Starting point "4 cores and 8GB per agent server"; "can handle 10-25 concurrent jobs, depending on the components in use" [L11] | Pipecat Cloud `agent-1x` default 0.5 vCPU / 1 GB for one session; `agent-2x` 1 vCPU / 2 GB; `agent-3x` 1.5 vCPU / 3 GB. One instance = one session [P6] |
| Vendor load test | Voice-to-voice, Silero VAD, 30 concurrent agents on one 4-core/8 GB machine: peak ~3.8 cores, ~2.8 GB [L11] | None found in official docs (sizing and production pages give no per-bot CPU/RAM figures) [P5][P14] |
| Load / admission control | Default `load_fnc` = 5 s average CPU; `load_threshold` 0.7 (fixed on LiveKit Cloud). Prewarmed processes default `ceil(cpu_count)` (Python) [L11][L17] | No built-in admission control found; guidance is predictive autoscaling, KEDA on an "active sessions" metric instead of CPU HPA; dev runner "has no way to scale past one host" [P5] |
| Self-hosting | LiveKit server (Apache-2.0, Go) plus agent workers; SIP service separate [L9][L12]. Agents Cloud is optional | Run the Python process anywhere; Pipecat Cloud (Daily) optional; Pipecat Enterprise offers self-hosted regions in your own cluster [P5][P14] |

## 2. Sizing arithmetic (INFERRED, not stated by sources)

- LiveKit: 4 cores / 8 GB for 10-25 jobs gives 0.16-0.4 core and 0.32-0.8 GB per job as the provisioning rule [L11]. The 30-agent load test gives ~0.13 core and ~93 MB per session (peak 3.8 cores / 30, 2.8 GB / 30). The LiveKit page itself does not state the per-session figures.
- Pipecat Cloud: 0.5 vCPU / 1 GB per session is the sizing of a billed container profile, not a measured minimum [P6].
- Grace period: LiveKit says voice apps "might require a 10+ minute grace period" [L11]; Pipecat notes Kubernetes default `terminationGracePeriodSeconds` is 30 s and sessions run "10+ minutes" [P5].

## 3. Independent benchmarks (2025-2026)

No independent, controlled benchmark of concurrent-call density (calls per core) for LiveKit Agents vs Pipecat was found in these searches. Everything below is vendor-run, single-author, or single-call latency. Treat as weak evidence.

| Source | What it measured | Frameworks | Result (as reported) | Caveat |
|---|---|---|---|---|
| todoforai `voice-agent-bench` [B1] | Black-box voice-to-voice latency, barge-in, echo. Fixed mock LLM, 5 conversations x 6 turns (n=30), median | Pipecat 1.8.1 only (LiveKit absent) | Pipecat 1.8.1 clean 1050 ms, hesitation 1290 ms, echo 1320 ms, talked through 2 of 12 overlaps, 20/30 self-interruptions. Deepgram+ElevenLabs Flash baseline 980 ms clean | No publication date on page; +/-300 ms jitter warned; single conversations, no concurrency or CPU data. Pipecat 1.8.1 is an old version vs 1.12.0 |
| Cekura (testing vendor) [B2][B3] | 82 caller scenarios x 3 repeats, agent-layer response time and reliability | LiveKit, Pipecat among 7-8 platforms | Both above 97% clean-connection rate [B2]. Pipecat mean response ~1.97 s (via search snippet of a Cekura article) [B4]. Telnyx page quotes Cekura v1 means: LiveKit 2.59 s, Pipecat 1.97 s [B5] | Vendor-authored; start point of timer "not fully specified" [B5]; 2.59 s not confirmed on a Cekura page I could read; per-framework table not visible on benchmarks.cekura.ai |
| Telnyx comparison [B5] | 100 concurrent calls over PSTN for hosted platforms | Pipecat/LiveKit not in the p95 sets | Hosted platforms only (ElevenLabs, Vapi, Retell, Bland, Telnyx) | Telnyx is a competitor vendor |
| Evalgent, Sellerity, Dograh blogs [B6][B7][B8] | Opinion/comparison posts | Both | Evalgent (Sept 2026): same providers, framework adds tens of ms; Sellerity: LiveKit 750-900 ms vs Pipecat 800-950 ms (Deepgram, GPT-4o, ElevenLabs, no methodology); Dograh restates LiveKit's 10-25 jobs per 4c/8GB with no method | Search-summary level only for Evalgent (fetch returned 429); not benchmarks |
| LiveKit SFU test (as cited in blogs) [B6] | 1 publisher, 3,000 subscribers, 92% CPU on 16-core | LiveKit server | Media server capacity, not agent sessions | Second-hand; not verified on LiveKit's page |
| Medium "100 concurrent calls" post [B9] | Title claims benchmark of open-source voice engines at 100 calls | unknown | UNVERIFIED: fetch returned 403, no content read | Do not cite numbers |

Conclusion for README: do not claim brisvox beats either framework in density on the basis of third-party data. The only official per-session figures are LiveKit's 4 core / 8 GB for 10-25 jobs (and its 30-agent test) and Pipecat Cloud's 0.5 vCPU / 1 GB per session profile. brisvox's own numbers must come from `bench/` with the methodology stated.

## 4. Gaps and UNVERIFIED items

- LiveKit audio turn detector `v1-mini`: CPU latency and RAM not stated [L8].
- LiveKit default `min_delay`/`max_delay` endpointing values: not stated on the turns page [L13].
- LiveKit parallel tool calls: not stated on the tools page [L15]. Pipecat MCP: not stated on the function-calling page [P13] (the README/other pages may cover it; not fetched).
- LiveKit `docs.livekit.io/.../*.md` URLs returned 404; the non-`.md` URL paths worked.
- Pipecat Cloud pricing and LiveKit Cloud pricing were not collected.
- Pipecat's LiveKit transport issue (multi-participant latency, GitHub issue #3218) was only seen as a search title, not read.
- Star counts are point-in-time (2026-10-14) from the GitHub API.

## 5. Sources (all fetched 2026-10-14)

LiveKit
- [L1] https://api.github.com/repos/livekit/agents/releases?per_page=8
- [L2] https://pypi.org/pypi/livekit-agents/json
- [L3] https://api.github.com/repos/livekit/agents-js/releases?per_page=3
- [L4] https://api.github.com/repos/livekit/agents
- [L5] https://api.github.com/repos/livekit/agents-js
- [L6] https://github.com/livekit/agents
- [L7] https://docs.livekit.io/agents/server/lifecycle/
- [L8] https://docs.livekit.io/agents/logic/turns/turn-detector/
- [L9] https://api.github.com/repos/livekit/livekit
- [L10] https://api.github.com/repos/livekit/sip
- [L11] https://docs.livekit.io/deploy/custom/deployments/
- [L12] https://docs.livekit.io/telephony/
- [L13] https://docs.livekit.io/agents/logic/turns/
- [L14] https://docs.livekit.io/agents/logic/turns/vad/
- [L15] https://docs.livekit.io/agents/logic/tools/
- [L16] https://docs.livekit.io/agents/models/realtime/
- [L17] https://docs.livekit.io/agents/server/options/

Pipecat
- [P1] https://api.github.com/repos/pipecat-ai/pipecat/releases?per_page=8 and /releases/latest
- [P2] https://pypi.org/pypi/pipecat-ai/json
- [P3] https://api.github.com/repos/pipecat-ai/pipecat
- [P4] https://github.com/pipecat-ai/pipecat
- [P5] https://docs.pipecat.ai/pipecat/deployment/running-bots-in-production.md
- [P6] https://docs.pipecat.ai/deployment/pipecat-cloud/fundamentals/deploy (profiles, via search result summary) and https://docs.pipecat.ai/pipecat-cloud/enterprise/sizing
- [P7] https://docs.pipecat.ai/pipecat/learn/transports.md
- [P8] https://docs.pipecat.ai/api-reference/server/utilities/turn-detection/smart-turn-overview.md
- [P9] https://docs.pipecat.ai/llms.txt (Krisp VIVA Turn page listed)
- [P10] https://github.com/pipecat-ai/smart-turn
- [P11] https://docs.pipecat.ai/api-reference/server/services/vad/silero-vad-analyzer.md
- [P12] https://docs.pipecat.ai/pipecat/fundamentals/interruptions.md
- [P13] https://docs.pipecat.ai/pipecat/learn/function-calling.md
- [P14] https://docs.pipecat.ai/pipecat-cloud/enterprise/sizing

Benchmarks and commentary
- [B1] https://github.com/todoforai/voice-agent-bench
- [B2] https://www.cekura.ai/blogs/pipecat-vs-livekit-the-real-difference
- [B3] https://benchmarks.cekura.ai
- [B4] https://www.cekura.ai/discover/pipecat-regression-testing-after-prompt-changes (search snippet only)
- [B5] https://telnyx.com/resources/voice-ai-agents-compared-latency
- [B6] https://www.evalgent.com/blog/pipecat-vs-livekit (search summary only)
- [B7] https://sellerity.co/blog/livekit-pipecat-web-voice-agents (search summary only)
- [B8] https://www.dograh.com/feeds/blog/scale-concurrent-calls
- [B9] https://medium.com/@nakshkanchan12/i-benchmarked-every-open-source-voice-ai-engine-at-100-concurrent-calls-a0c8ad2722c9 (403, unread)
