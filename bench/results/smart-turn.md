# Smart Turn v3.2 latency

Date: 2026-10-08. Machine: AMD Ryzen 9 9950X (16 cores / 32 threads), Windows 11 Pro, shared
desktop (not isolated). Model: smart-turn-v3.2-cpu.onnx (int8, SHA-256 2bb02631...e4f).
One thread everywhere (ONNX Runtime intra-op 1, inter-op 1, sequential, all optimizations),
8 s of 16 kHz audio, 15 warm runs after one warm-up, median (max).

| path | features | model |
|---|---:|---:|
| Rust: brisvox `WhisperLogMel` + `ort` 2.0.0-rc.13 (onnxruntime 1.28.0) | 1.00 ms (1.20) | 22.5 ms (25.5) |
| Python: Pipecat numpy extractor (numpy 2.5.3) + onnxruntime 1.28.0 | 2.12 ms (2.61) | 24.2 ms (25.3) |

Commands:
- Rust: `cargo test --release -p brisvox-inference --all-features --test smart_turn --
  --nocapture --test-threads=1 features_and_model_latency`
- Python: `python scripts/models/bench_smart_turn.py` (venv with onnxruntime==1.28.0, numpy)

Other runs of the Rust test on the same machine, with other tests in parallel: features
1.16-1.25 ms, model 26-28.4 ms. On the inference pool (one thread, after `warm(1)`), one
analysis (window + features + model) took 25-35 ms; pool wait 8-24 us. The caller-side ring
copy (`SmartTurn::analyze` before the pool hop) takes 1.5 us (release, `the_caller_side_copy_is_cheap`).

Not yet measured: the Linux target container with CPU limits, cold start
(session creation ~100 ms, avoided by `SmartTurnOrt::warm`).
