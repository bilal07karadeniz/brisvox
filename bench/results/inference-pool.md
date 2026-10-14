# Inference pool load test

- command: `brisvox-bench infer-load --seconds 4 --costs-us 100,200,250,280 --out bench/results/inference-pool.md`
- brisvox 0.1.0 (release build), windows x86_64, 32 logical cores, 2 tokio workers (threads, not a CPU limit: other threads may use any core)
- inference pool with 4 OS threads; sessions on the tokio runtime; jobs busy-spin (real CPU), 4 s per row

Each session submits one job per period (32 ms = one Silero VAD window); a job busy-spins `job cost` of CPU. Arrivals: `timers` = each session a tokio task waiting on a timer and on its job (closed loop; bursty on Windows, see tick late), `paced` = one thread spin-paces jobs evenly (open loop; measures the pool itself). Offered load = job CPU per second / pool threads. Wait = queued to started; latency = submit to result. Times in ms.

| arrivals | sessions | pool threads | job cost | offered load | utilisation | jobs done | rejected | timed out | missed windows | wait p50 | wait p99 | wait max | latency p99 | max queue | tick late p50 | tick late p99 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| timers | 500 | 4 | 0.100 | 0.39 | 0.40 | 62500 | 0 | 0 | 0/62500 | 1.899 | 4.160 | 5.731 | 4.302 | 226 | 2.199 | 10.533 |
| timers | 500 | 4 | 0.200 | 0.78 | 0.79 | 62500 | 0 | 0 | 0/62500 | 3.804 | 11.397 | 12.524 | 11.668 | 248 | 0.719 | 12.857 |
| timers | 500 | 4 | 0.250 | 0.98 | 0.98 | 62500 | 0 | 0 | 0/62500 | 0.796 | 14.396 | 15.381 | 14.742 | 244 | 0.600 | 1.237 |
| timers | 500 | 4 | 0.280 | 1.09 | 0.99 | 56639 | 0 | 0 | 5861/62500 | 18.669 | 33.809 | 34.758 | 34.120 | 487 | 0.606 | 1.216 |
| paced | 500 | 4 | 0.100 | 0.39 | 0.39 | 62500 | 0 | 0 | 0/62500 | 0.008 | 0.111 | 2.433 | 0.302 | 85 | 0.000 | 0.055 |
| paced | 500 | 4 | 0.200 | 0.78 | 0.79 | 62500 | 0 | 0 | 0/62500 | 0.007 | 0.620 | 5.597 | 0.926 | 90 | 0.000 | 0.022 |
| paced | 500 | 4 | 0.250 | 0.98 | 0.98 | 62500 | 0 | 0 | 0/62500 | 0.098 | 1.141 | 1.701 | 1.413 | 27 | 0.000 | 0.017 |
| paced | 500 | 4 | 0.280 | 1.09 | 0.99 | 57543 | 4957 | 0 | 0/62500 | 72.287 | 73.379 | 73.686 | 73.684 | 1024 | 0.000 | 0.024 |
