# Listening-test load steps on Railway, 2026-10-15

Setup: `bench/railway/README.md`. Railway, region europe-west4. Each server container limited to
4 vCPU / 4 GB (cgroup `cpu.max` 4.0, `memory.max` 3.73 GiB as read inside the container) on
AMD EPYC 9655P (`/proc/cpuinfo`: avx2, avx512f, avx512_vnni, avx_vnni). The
synthetic calls run the real provider client code of each system against the fake providers over
TLS (Deepgram live STT, OpenAI-compatible chat, Deepgram Aura), the real Silero VAD and Smart Turn
v3.2 models, the agent `bench/fake_providers/agent.fakes.toml` (adaptive turns, 3 s fallback
silence). Each synthetic caller plays `fixtures/en_two_utterances` (two questions), hangs up after
the second answer and dials again. No real call was running during these steps.

Numbers: the load generator's view over the last 30 s of each step (two readings per step, 60 s and
90 s after the step started; the table shows the second, and the CPU column both; the 350 step has
one reading). V2V = end of the caller's speech to the
first audible reply. The fakes' floor is about 860 ms (STT final 150 ms after 300 ms endpointing,
LLM first token 350 ms, TTS first audio 150 ms; `bench/fake_providers/README.md`). The load
generator sent 0 late frames in every step (it kept up).

## brisvox (one `brisvox-server` process for the load)

| calls | answered turns | V2V p50 | V2V p95 | V2V max | refused dials | container CPU (cores) | container memory |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 50 | 200 | 870 ms | 875 ms | 879 ms | 0 | 2.8 | 258 MB |
| 100 | 400 | 870 ms | 873 ms | 882 ms | 0 | 0.9-1.1 | 317 MB |
| 150 | 600 | 870 ms | 878 ms | 889 ms | 0 | 2.2-3.5 | 385 MB |
| 200 | 800 | 873 ms | 1194 ms | 1342 ms | 0 | 2.5-2.8 | 422 MB |
| 250 | 1000 | 1213 ms | 1324 ms | 1388 ms | 0 | 1.6-3.3 | 526 MB |
| 300 | 1256 | 1265 ms | 2129 ms | 3735 ms | 967 | 3.7-4.0 | 482 MB |
| 350 | 1382 | 3627 ms | 3796 ms | 3899 ms | 1162 | 1.4 (one sample) | 487 MB |

- Up to 150 calls the V2V is the fakes' floor plus about 10 ms. From 200 the tail grows; at 250 the
  first reading had a p95 of 3.6 s. At 300 the 4 vCPU are used up: the server refuses new dials
  with 503 (pool pressure) instead of taking calls it cannot serve, and turns whose Smart Turn
  analysis is late end on the 3 s fallback silence (the ~3.6 s V2V values).
- The CPU column is one 2 s sample at the reading; it swings with the calls' phases.
- **Before the fix of this date** the pool-pressure guard judged one instantaneous queue sample and
  refused dials on an idle pool: 64 refusals while 50 calls dialed within a second (0.36 cores
  busy), 319 at 150 calls with about 1 core busy. The queue depth now counts only once it stayed
  over its limit for 100 ms (`BRISVOX_OVERLOAD_QUEUE_CONFIRM`, `docs/OPERATIONS.md`); the table
  above is after the fix.

## Pipecat 1.12 (one `bot.py` with 4 uvicorn workers for the load)

These steps ran at the same time as the first brisvox series (separate containers, same load
generator).

| calls | answered turns | missed turns | V2V p50 | V2V p95 | V2V max | failed calls | container memory |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 5 | 12 | 0 | 1385 ms | 1417 ms | 1417 ms | 0 | 2.05 GB |
| 10 | 38 | 0 | 1281 ms | 1711 ms | 1788 ms | 0 | 3.69 GB |
| 15 | 54 | 2 | 1264 ms | 2425 ms | 2529 ms | 2 | 3.69 GB |
| 20 | 60 | 2 | 1486 ms | 2771 ms | 2946 ms | 4 | 2.95 GB |
| 25 | 69 | 1 | 2459 ms | 9782 ms | 13069 ms | 16 | 2.97 GB |
| 30 | 98 | 2 | 2180 ms | 9183 ms | 10789 ms | 27 | 1.82 GB |

- At 10 calls the container is at its memory limit. From 15 calls uvicorn workers are restarted
  over and over (the Pipecat start-up banner repeats in the log every 10 to 30 s) and their calls
  break ("connection reset by peer"); memory drops after each restart, which is why it falls at
  20 to 30 calls. At 25 and more the p95 is near 10 s.
- The same per-call memory showed in the capacity benchmark (Pipecat at 20 calls: 4.0 GB PSS,
  `docs/LATENCY.md`).

## What this does and does not show

- It is one run per step on a shared cloud host; read the trends, not the last digit.
- The fakes are idealised providers with fixed latencies; real providers add their own variance
  on top (the listening test's real call shows that part).
- Pipecat's per-call memory depends on its configuration (one Smart Turn and one Silero session
  per call; fewer workers use less memory but fewer cores). The worker count follows the capacity
  benchmark (one per vCPU).
