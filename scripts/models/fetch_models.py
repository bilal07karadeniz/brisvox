#!/usr/bin/env python3
"""Fetch the on-device models brisvox needs, pinned and checksum-verified.

Models are not committed (size, separate licenses); this script downloads them into `models/`
(git-ignored) and refuses any file whose SHA-256 differs from the pinned value.

Usage (from the repository root):
    python3 scripts/models/fetch_models.py           # fetch missing models
    python3 scripts/models/fetch_models.py --check   # verify present models, fetch nothing

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
    args = parser.parse_args()
    os.makedirs(MODELS, exist_ok=True)
    ok = True
    for name, (url, sha256, size, license_) in PINNED.items():
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
