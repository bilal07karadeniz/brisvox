"""How far apart do correct implementations of the Smart Turn graph land?

Runs the 12 golden cases (fixtures/smart_turn_golden.json) through the same int8 QDQ model in
several execution plans and prints the largest probability difference to the default plan
(onnxruntime ORT_ENABLE_ALL, the one the goldens come from):

  * onnxruntime at ORT_ENABLE_EXTENDED / ORT_ENABLE_BASIC / ORT_DISABLE_ALL (BASIC and below
    keep QuantizeLinear/DequantizeLinear as separate float nodes; EXTENDED and ALL fuse them
    into integer QLinear kernels), with 4 intra-op threads, and
  * the ONNX reference evaluator (numpy, literal graph semantics; opset raised to 21 because
    the evaluator has no DequantizeLinear-18).

Result (2026-10-13, onnxruntime 1.28.0): see docs/providers/smart-turn.md, "Pure-Rust backend".
Needs python with onnxruntime==1.28.0, onnx, numpy, models/smart-turn-v3.2-cpu.onnx and the
Pipecat feature extractor (downloaded by make_smart_turn_goldens.py into target/model-cache).
Set BRISVOX_MODELS_DIR when models/ lives elsewhere (a git worktree). Writes nothing.

tract: the Rust harness `cargo run --release -p brisvox-inference --features silero-tract
--example smart_turn_tract -- --json tract.json` (crates/brisvox-inference/examples/) prints
tract's own numbers against the goldens and writes its per-case probabilities; with
BRISVOX_TRACT_JSON=tract.json this script also compares them with every other plan.
"""

import importlib.util
import json
import os
import wave

import numpy as np
import onnx
import onnxruntime as ort
from onnx.reference import ReferenceEvaluator

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
MODEL = os.path.join(os.environ.get("BRISVOX_MODELS_DIR", os.path.join(ROOT, "models")), "smart-turn-v3.2-cpu.onnx")
WINDOW = 8 * 16_000


def load_goldens_module():
    spec = importlib.util.spec_from_file_location("goldens", os.path.join(HERE, "make_smart_turn_goldens.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def case_features(golden, features_of):
    out = {}
    for key, case in golden["cases"].items():
        with wave.open(os.path.join(ROOT, "fixtures", case["fixture"] + ".wav"), "rb") as w:
            audio = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").astype(np.float32) / 32768.0
        x = audio[case["start"]:case["end"]]
        x = x[-WINDOW:] if len(x) > WINDOW else np.pad(x, (WINDOW - len(x), 0))
        out[key] = features_of(x, do_normalize=True)
    return out


def ort_probabilities(features, level, threads):
    options = ort.SessionOptions()
    options.graph_optimization_level = level
    options.intra_op_num_threads = threads
    options.inter_op_num_threads = 1
    session = ort.InferenceSession(MODEL, sess_options=options, providers=["CPUExecutionProvider"])
    return {k: float(session.run(None, {"input_features": f[None]})[0].reshape(-1)[0]) for k, f in features.items()}


def reference_probabilities(features):
    model = onnx.load(MODEL)
    for opset in model.opset_import:
        if opset.domain == "":
            opset.version = 21
    evaluator = ReferenceEvaluator(model)
    return {k: float(evaluator.run(None, {"input_features": f[None]})[0].reshape(-1)[0]) for k, f in features.items()}


def compare(name, probabilities, base):
    diffs = [abs(probabilities[k] - base[k]) for k in base]
    flips = sum((probabilities[k] > 0.5) != (base[k] > 0.5) for k in base)
    print(f"{name:28} max|d|={max(diffs):.4f} mean|d|={sum(diffs) / len(diffs):.4f} "
          f"cases>1e-3={sum(d > 1e-3 for d in diffs):2}/12 verdict flips={flips}")


def compare_pair(name, probabilities, other_name, other):
    diffs = [abs(probabilities[k] - other[k]) for k in other]
    print(f"{name:28} vs {other_name:22} max|d|={max(diffs):.4f}")


def main():
    goldens = load_goldens_module()
    golden = json.load(open(os.path.join(ROOT, "fixtures", "smart_turn_golden.json"), encoding="utf-8"))
    features = case_features(golden, goldens.load_extractor())
    levels = ort.GraphOptimizationLevel
    base = ort_probabilities(features, levels.ORT_ENABLE_ALL, 1)
    print(f"onnxruntime {ort.__version__}; base = ORT_ENABLE_ALL, 1 thread (the goldens)")
    compare("ORT_ENABLE_ALL, 4 threads", ort_probabilities(features, levels.ORT_ENABLE_ALL, 4), base)
    compare("ORT_ENABLE_EXTENDED", ort_probabilities(features, levels.ORT_ENABLE_EXTENDED, 1), base)
    compare("ORT_ENABLE_BASIC", ort_probabilities(features, levels.ORT_ENABLE_BASIC, 1), base)
    compare("ORT_DISABLE_ALL", ort_probabilities(features, levels.ORT_DISABLE_ALL, 1), base)
    reference = reference_probabilities(features)
    compare("ONNX reference evaluator", reference, base)
    tract_json = os.environ.get("BRISVOX_TRACT_JSON")
    if tract_json:
        basic = ort_probabilities(features, levels.ORT_ENABLE_BASIC, 1)
        with open(tract_json, encoding="utf-8") as f:
            tract = json.load(f)
        for plan in ("typed", "optimized"):
            compare(f"tract {plan}", tract[plan], base)
            compare_pair(f"tract {plan}", tract[plan], "ORT_ENABLE_BASIC", basic)
            compare_pair(f"tract {plan}", tract[plan], "ONNX reference evaluator", reference)


if __name__ == "__main__":
    main()
