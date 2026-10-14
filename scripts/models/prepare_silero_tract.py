#!/usr/bin/env python3
"""Prepare a tract-loadable copy of Silero VAD v6.2.3.

tract cannot infer shapes inside the stock graph's nested `If` subgraphs. This script:
1. replaces the top-level sample-rate `If` with its 16 kHz branch (inputs input/state/sr kept;
   sr becomes unused) and fixes static shapes: input [1, 576], state [2, 1, 128];
2. lets onnxruntime constant-fold the result offline (ORT_ENABLE_BASIC), which removes the
   remaining nested `If` nodes;
3. writes models/silero_vad_16k_tract.onnx and prints its SHA-256 (pin it in fetch_models.py).

Needs python with `onnx` and `onnxruntime` (model preparation only, not at engine build or run
time) and models/silero_vad.onnx (scripts/models/fetch_models.py).

Reproducibility: the output is byte-identical across runs with onnxruntime 1.30.0 / onnx 1.23.2
(2026-10-08): 1251444 bytes, SHA-256
9efbb7014ded92ccd0b1ebad60a7fcd441e13c2f86ed66089219cad519accb8f. Other onnxruntime versions may
fold differently: re-pin after checking the golden test.

Usage (from the repository root):
    python scripts/models/prepare_silero_tract.py
"""

import hashlib
import os
import tempfile

import onnx
import onnxruntime as ort
from onnx import helper

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
SRC = os.path.join(ROOT, "models", "silero_vad.onnx")
DST = os.path.join(ROOT, "models", "silero_vad_16k_tract.onnx")


def set_dims(value_info, dims):
    shape = value_info.type.tensor_type.shape
    shape.ClearField("dim")
    for d in dims:
        shape.dim.add().dim_value = d


def inline_16k(model):
    graph = model.graph
    dispatch = next(n for n in graph.node if n.op_type == "If")
    then_graph = next(a.g for a in dispatch.attribute if a.name == "then_branch")
    dispatch_outputs = set(dispatch.output)
    kept = [
        n
        for n in graph.node
        if n.op_type != "If" and not any(i in dispatch_outputs for i in n.input)
    ]
    nodes = kept + list(then_graph.node)
    for src_name, dst_name in zip((o.name for o in then_graph.output), ("output", "stateN")):
        nodes.append(helper.make_node("Identity", [src_name], [dst_name], name=f"final_{dst_name}"))
    inlined = helper.make_graph(
        nodes,
        "silero_vad_16k",
        list(graph.input),
        list(graph.output),
        initializer=list(graph.initializer) + list(then_graph.initializer),
    )
    for vi in inlined.input:
        set_dims(vi, {"input": [1, 576], "state": [2, 1, 128], "sr": []}[vi.name])
    for vi in inlined.output:
        set_dims(vi, {"output": [1, 1], "stateN": [2, 1, 128]}[vi.name])
    out = helper.make_model(inlined, opset_imports=model.opset_import, producer_name="brisvox")
    out.ir_version = model.ir_version
    onnx.checker.check_model(out)
    return out


def main():
    model = onnx.load(SRC)
    with tempfile.TemporaryDirectory() as tmp:
        inlined_path = os.path.join(tmp, "inlined.onnx")
        onnx.save(inline_16k(model), inlined_path)
        options = ort.SessionOptions()
        options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_BASIC
        options.optimized_model_filepath = DST + ".tmp"
        options.intra_op_num_threads = 1
        ort.InferenceSession(inlined_path, sess_options=options, providers=["CPUExecutionProvider"])
    os.replace(DST + ".tmp", DST)
    folded = onnx.load(DST)
    ops = sorted({n.op_type for n in folded.graph.node})
    assert "If" not in ops, ops
    digest = hashlib.sha256(open(DST, "rb").read()).hexdigest()
    print(f"wrote {DST}: {os.path.getsize(DST)} bytes, sha256 {digest}")
    print(f"onnxruntime {ort.__version__}, onnx {onnx.__version__}, ops {ops}")


if __name__ == "__main__":
    main()
