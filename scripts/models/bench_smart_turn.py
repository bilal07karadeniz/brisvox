#!/usr/bin/env python3
"""Python reference timing of Smart Turn v3.2, for comparison with the Rust path.

Pipecat's pipeline on one thread: the vendored numpy log-mel extractor (Pipecat commit pinned in
make_smart_turn_goldens.py), then onnxruntime with Pipecat's session options. Prints the median
and max of 15 warm runs of each part. The Rust numbers come from
`cargo test --release -p brisvox-inference --all-features --test smart_turn -- --nocapture
features_and_model_latency`; both are recorded in bench/results/smart-turn.md.

Needs python with onnxruntime (1.28.0, like the goldens) + numpy, models/smart-turn-v3.2-cpu.onnx.
Usage (from the repository root): python scripts/models/bench_smart_turn.py
"""

import importlib.util
import os
import time

import numpy as np
import onnxruntime as ort

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("goldens", os.path.join(HERE, "make_smart_turn_goldens.py"))
goldens = importlib.util.module_from_spec(spec)
spec.loader.exec_module(goldens)

RUNS = 15


def main():
    features_of = goldens.load_extractor()
    options = ort.SessionOptions()
    options.intra_op_num_threads = 1
    options.inter_op_num_threads = 1
    options.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
    options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    session = ort.InferenceSession(goldens.MODEL, sess_options=options, providers=["CPUExecutionProvider"])
    audio = (np.random.default_rng(0).standard_normal(goldens.WINDOW) * 0.05).astype(np.float32)
    features = features_of(audio, do_normalize=True)
    session.run(None, {"input_features": features[np.newaxis]})
    feature_times, model_times = [], []
    for _ in range(RUNS):
        start = time.perf_counter()
        features = features_of(audio, do_normalize=True)
        middle = time.perf_counter()
        session.run(None, {"input_features": features[np.newaxis]})
        end = time.perf_counter()
        feature_times.append(middle - start)
        model_times.append(end - middle)
    feature_times.sort()
    model_times.sort()
    print(f"python + onnxruntime {ort.__version__}, numpy {np.__version__}, {RUNS} runs: "
          f"features median {feature_times[RUNS // 2] * 1e3:.2f} ms (max {feature_times[-1] * 1e3:.2f}), "
          f"model median {model_times[RUNS // 2] * 1e3:.2f} ms (max {model_times[-1] * 1e3:.2f})")


if __name__ == "__main__":
    main()
