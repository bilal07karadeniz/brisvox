# Capacity: Smart Turn published graph vs fused graph (2026-10-23)

`bench/capacity_linux.sh` with `PROVIDERS=fakes MODELS=1 SYSTEMS=brisvox CPUS=4 MEM=4g`: brisvox-server
with its real provider clients against `bench/fake_providers` (860 ms floor), the real Silero VAD and Smart
Turn v3.2 models, SLO p95 V2V <= 1110 ms, no failed call. The only difference between the two sides is
`BRISVOX_SMART_TURN_GRAPH` (`original`: the pinned file as published, the default; `fused`: opt-in, GELUs fused in
memory, docs/providers/smart-turn.md "Fused GELU"). Runs interleaved in pairs, the order swapped in every
other pair. Host: the reference desktop (16 cores, one DDR5 channel), WSL2. Pairs 1-4 ran while other
programs used the CPU and memory (pairs 3 and 4 heavily; a first pair-3 attempt, where the published-graph
run failed at 20 calls on a host stall, was discarded and repeated); pairs 5 and 6 ran on a quiet host.

| pair | host | fused: max calls within the SLO | published: max calls within the SLO |
|---|---|---|---|
| 1 | busy | 260 | 230 |
| 2 | busy | 240 | 200 |
| 3 | very busy | 190 | 170 |
| 4 | very busy | 180 | 200 |
| 5 | quiet | 290 | 270 |
| 6 | quiet | 320 | 270 |
| median, all | | **250** | **215** |
| median, quiet | | **305** | **270** |

Server CPU (cores, as measured by the harness) at the same load, pairs 5 and 6 (quiet host):

| calls | 40 | 80 | 120 | 160 | 200 |
|---|---|---|---|---|---|
| fused | 0.46 | 0.89 | 1.23 | 1.45 | 1.51 |
| published | 0.47 | 0.91 | 1.25 | 1.48 | 1.53 |

Reading: the fused graph held more calls in 5 of 6 pairs (on the quiet host 305 against 270, about 13%);
below the knee the CPU is the same, the gain is at the top, where concurrent analyses compete for memory
bandwidth. The run-to-run spread on this host is +-20 calls on a quiet host and much more on a busy one.

Regression check, same day and host (quiet): the released 0.1.1 code (the code of the 2026-10-16 desktop
sweep) against this code with the published graph, interleaved: 250 and 280 calls against 260 and 270, CPU at 200 calls
1.65 and 1.54 against 1.58 and 1.57 cores (`capacity-brisvox-v011-*`, `capacity-brisvox-master-original-*`).
No regression.

Per-run files: `capacity-brisvox-graph-{fused,original}-fakes-models-linux-2026-10-23-run{1..6}.{md,json,server.log}`.
