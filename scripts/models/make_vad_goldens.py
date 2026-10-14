#!/usr/bin/env python3
"""Golden Silero VAD probabilities for the Rust implementation's tests.

Runs the reference streaming algorithm (silero-vad OnnxWrapper: 512 new samples + 64 samples of
context per call at 16 kHz, state carried) with the official onnxruntime on the committed
fixtures, and writes fixtures/vad_golden.json: {fixture: {sample_rate, window, context, probs}}.

Needs: python3 with `onnxruntime` and `numpy` (e.g. a venv: pip install onnxruntime numpy), and
models/silero_vad.onnx (scripts/models/fetch_models.py). Generated 2026-10-08 with
onnxruntime 1.30.0, one intra/inter-op thread.

Usage (from the repository root):
    python scripts/models/make_vad_goldens.py
"""

import json
import os
import wave

import numpy as np
import onnxruntime as ort

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
MODEL = os.path.join(ROOT, "models", "silero_vad.onnx")
FIXTURES = os.path.join(ROOT, "fixtures")
OUT = os.path.join(FIXTURES, "vad_golden.json")
NAMES = ["en_two_utterances", "tr_two_utterances", "en_tr_mixed"]
WINDOW, CONTEXT, RATE = 512, 64, 16_000


def main():
    options = ort.SessionOptions()
    options.intra_op_num_threads = 1
    options.inter_op_num_threads = 1
    session = ort.InferenceSession(MODEL, sess_options=options, providers=["CPUExecutionProvider"])
    golden = {}
    for name in NAMES:
        with wave.open(os.path.join(FIXTURES, f"{name}.wav"), "rb") as w:
            assert w.getframerate() == RATE and w.getnchannels() == 1 and w.getsampwidth() == 2
            audio = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").astype(np.float32) / 32768.0
        state = np.zeros((2, 1, 128), dtype=np.float32)
        context = np.zeros((1, CONTEXT), dtype=np.float32)
        probs = []
        for start in range(0, len(audio) - WINDOW + 1, WINDOW):
            x = np.concatenate([context, audio[start : start + WINDOW].reshape(1, WINDOW)], axis=1)
            out, state = session.run(None, {"input": x, "state": state, "sr": np.array(RATE, dtype=np.int64)})
            context = x[:, -CONTEXT:]
            probs.append(round(float(out.reshape(-1)[0]), 7))
        golden[name] = {"sample_rate": RATE, "window": WINDOW, "context": CONTEXT, "probs": probs}
        print(f"{name}: {len(probs)} windows")
    text = json.dumps(golden, separators=(",", ":")) + "\n"
    tmp = OUT + ".tmp"
    with open(tmp, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    os.replace(tmp, OUT)
    print("wrote", OUT)


if __name__ == "__main__":
    main()
