#!/usr/bin/env python3
"""Golden Smart Turn v3.2 outputs for the Rust implementation's tests.

Reproduces Pipecat's LocalSmartTurnAnalyzerV3._predict_endpoint exactly:
  keep the last 8 s of 16 kHz audio, or zero-pad at the START to 8 s;
  log-mel features with Pipecat's vendored numpy Whisper extractor (fetched at a pinned commit);
  run models/smart-turn-v3.2-cpu.onnx; probability = outputs[0][0].

Cases, on the committed fixtures: for each speech segment, the audio from 500 ms before its start
(Pipecat's pre_speech_ms) to its end ("complete"), and the same cut at 55% of the segment
("cut" - usually incomplete). Writes:
  fixtures/smart_turn_golden.json  {case: {fixture, start, end, probability, features_fnv1a64}}
                                   (FNV-1a 64 of the case's (80, 800) f32 LE features: the Rust
                                   extractor must reproduce every case bit for bit)
  fixtures/smart_turn_features.f32 the full (80, 800) feature matrix of the first case, f32 LE

Must run with onnxruntime ORT_VERSION, the version the Rust `ort` crate bundles (ort-sys
2.0.0-rc.13 downloads 1.28.0): this int8 model's output changes by up to 0.16 between runtime
versions on identical input (1.28.0 vs 1.30.0 measured 2026-10-08, docs/providers/smart-turn.md).
Needs python with onnxruntime + numpy, models/smart-turn-v3.2-cpu.onnx
(scripts/models/fetch_models.py). Usage (from the repository root):
    python scripts/models/make_smart_turn_goldens.py
"""

import hashlib
import importlib.util
import json
import os
import urllib.request
import wave

import numpy as np
import onnxruntime as ort

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
MODEL = os.path.join(ROOT, "models", "smart-turn-v3.2-cpu.onnx")
FIXTURES = os.path.join(ROOT, "fixtures")
CACHE = os.path.join(ROOT, "target", "model-cache")
PIPECAT_COMMIT = "3b2dccd843c65d1632631ceac248a5128fb32242"
FEATURES_URL = (
    f"https://raw.githubusercontent.com/pipecat-ai/pipecat/{PIPECAT_COMMIT}"
    "/src/pipecat/audio/turn/smart_turn/_whisper_features.py"
)
RATE, WINDOW = 16_000, 8 * 16_000
ORT_VERSION = "1.28.0"
NAMES = ["en_two_utterances", "tr_two_utterances", "en_tr_mixed"]


def load_extractor():
    os.makedirs(CACHE, exist_ok=True)
    path = os.path.join(CACHE, f"whisper_features_{PIPECAT_COMMIT[:12]}.py")
    if not os.path.exists(path):
        request = urllib.request.Request(FEATURES_URL, headers={"User-Agent": "brisvox-models"})
        with urllib.request.urlopen(request, timeout=60) as response:
            data = response.read()
        with open(path + ".tmp", "wb") as f:
            f.write(data)
        os.replace(path + ".tmp", path)
    spec = importlib.util.spec_from_file_location("pipecat_whisper_features", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.compute_whisper_log_mel_features


def fnv1a64(data):
    h = 0xCBF29CE484222325
    for b in data:
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{h:016x}"


def last_8_seconds(audio):
    if len(audio) > WINDOW:
        return audio[-WINDOW:]
    return np.pad(audio, (WINDOW - len(audio), 0), mode="constant")


def main():
    if ort.__version__ != ORT_VERSION:
        raise SystemExit(f"onnxruntime {ort.__version__}: install onnxruntime=={ORT_VERSION} (see the docstring)")
    features_of = load_extractor()
    options = ort.SessionOptions()
    options.intra_op_num_threads = 1
    options.inter_op_num_threads = 1
    session = ort.InferenceSession(MODEL, sess_options=options, providers=["CPUExecutionProvider"])
    golden = {"model_sha256": hashlib.sha256(open(MODEL, "rb").read()).hexdigest(),
              "pipecat_commit": PIPECAT_COMMIT, "onnxruntime": ort.__version__, "cases": {}}
    first = None
    for name in NAMES:
        with wave.open(os.path.join(FIXTURES, f"{name}.wav"), "rb") as w:
            audio = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").astype(np.float32) / 32768.0
        meta = json.load(open(os.path.join(FIXTURES, f"{name}.json"), encoding="utf-8"))
        for i, seg in enumerate(meta["segments"]):
            begin = max(0, seg["start"] - RATE // 2)
            for kind, end in (("complete", seg["end"]), ("cut", seg["start"] + (seg["end"] - seg["start"]) * 55 // 100)):
                features = features_of(last_8_seconds(audio[begin:end]), do_normalize=True)
                out = session.run(None, {"input_features": features[np.newaxis, :, :]})
                probability = float(np.asarray(out[0]).reshape(-1)[0])
                key = f"{name}/{i}/{kind}"
                golden["cases"][key] = {
                    "fixture": name, "start": begin, "end": end, "probability": round(probability, 7),
                    "features_fnv1a64": fnv1a64(np.ascontiguousarray(features, dtype="<f4").tobytes()),
                }
                if first is None:
                    first = (key, features)
                print(f"{key}: p={probability:.4f}")
    key, features = first
    golden["features_case"] = key
    golden["features_shape"] = list(features.shape)
    with open(os.path.join(FIXTURES, "smart_turn_features.f32"), "wb") as f:
        f.write(np.ascontiguousarray(features, dtype="<f4").tobytes())
    with open(os.path.join(FIXTURES, "smart_turn_golden.json"), "w", encoding="utf-8", newline="\n") as f:
        json.dump(golden, f, indent=1)
        f.write("\n")
    print("graph inputs:", [(i.name, i.type, i.shape) for i in session.get_inputs()])
    print("graph outputs:", [(o.name, o.type, o.shape) for o in session.get_outputs()])


if __name__ == "__main__":
    main()
