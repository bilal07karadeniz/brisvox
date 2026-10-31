#!/usr/bin/env python3
"""Fetch the on-device models brisvox needs, pinned and checksum-verified.

Models are not committed (size, separate licenses); this script downloads them into `models/`
(git-ignored) and refuses any file whose SHA-256 differs from the pinned value.

Usage (from the repository root):
    python3 scripts/models/fetch_models.py           # fetch missing models
    python3 scripts/models/fetch_models.py --check   # verify present models, fetch nothing
    python3 scripts/models/fetch_models.py --evaluation   # also the evaluation-only models below

Standard library only.
"""

import argparse
import hashlib
import os
import sys
import urllib.request

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
MODELS = os.path.join(ROOT, "models")

# name -> (url, sha256, size, license). Sources and verification: docs/providers/silero-vad.md.
PINNED = {
    "silero_vad.onnx": (
        "https://raw.githubusercontent.com/snakers4/silero-vad/v6.2.3/src/silero_vad/data/silero_vad.onnx",
        "1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3",
        2327524,
        "MIT (Silero Team), https://github.com/snakers4/silero-vad/blob/v6.2.3/LICENSE",
    ),
    "smart-turn-v3.2-cpu.onnx": (
        # Pinned to the Hugging Face file whose LFS SHA-256 is checked below (docs/providers/smart-turn.md).
        "https://huggingface.co/pipecat-ai/smart-turn-v3/resolve/main/smart-turn-v3.2-cpu.onnx",
        "2bb026316b14a660486a75b1733cd3fbab8c2fd0314dc9af7be49f8cca967e4f",
        8679182,
        "BSD-2-Clause (Daily / pipecat-ai), Whisper-derived (MIT, OpenAI)",
    ),
}

# Evaluation-only models (`--evaluation`): used by `brisvox-bench denoise` to size what a better noise
# suppressor would cost and gain; the server never loads them. FastEnhancer-T, waveform graph with
# streaming caches (hop 256 samples, 16 kHz), trained on DNS Challenge 3 wideband data. The code is MIT
# (https://github.com/aask1357/fastenhancer/blob/main/LICENSE) but the release files carry no license
# statement of their own, so the weights are NOT cleared for shipping: docs/providers/noise-suppression.md,
# "Verified on 2026-10-30".
EVALUATION = {
    "fastenhancer_t.onnx": (
        "https://github.com/aask1357/fastenhancer/releases/download/onnx-dns-v1.0.0/fastenhancer_t.onnx",
        "a64663672b31bd1445502ee23328d291af13fb312e11bb574c010e7261a5ee7d",
        125233,
        "UNCLEAR (repo MIT; the release files state no license; evaluation only)",
    ),
}


def sha256_of(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def fetch(url, size):
    request = urllib.request.Request(url, headers={"User-Agent": "brisvox-models"})
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(size + 1)
    if len(data) != size:
        raise SystemExit(f"{url}: got {len(data)} bytes, expected {size}")
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="verify only, do not download")
    parser.add_argument(
        "--evaluation", action="store_true", help="also the evaluation-only models (license unclear)"
    )
    args = parser.parse_args()
    os.makedirs(MODELS, exist_ok=True)
    ok = True
    wanted = dict(PINNED)
    if args.evaluation:
        wanted.update(EVALUATION)
    for name, (url, sha256, size, license_) in wanted.items():
        path = os.path.join(MODELS, name)
        if os.path.exists(path):
            actual = sha256_of(path)
            if actual != sha256:
                print(f"{name}: SHA-256 mismatch ({actual}); delete it and fetch again", file=sys.stderr)
                ok = False
            else:
                print(f"{name}: ok ({license_})")
            continue
        if args.check:
            print(f"{name}: missing (run without --check to fetch)", file=sys.stderr)
            ok = False
            continue
        data = fetch(url, size)
        actual = hashlib.sha256(data).hexdigest()
        if actual != sha256:
            raise SystemExit(f"{name}: downloaded SHA-256 {actual} != pinned {sha256}")
        tmp = path + ".tmp"
        with open(tmp, "wb") as f:
            f.write(data)
        os.replace(tmp, path)
        print(f"{name}: fetched and verified ({license_})")
    if not ok:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
