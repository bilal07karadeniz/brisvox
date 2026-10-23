#!/usr/bin/env python3
"""Build-time download for the sherpa-onnx image (stdlib only, run by the Dockerfile).

Fetches the streaming server script and its helper at a pinned release tag, and the English
streaming model, verifying the SHA-256 of every download. The build fails on a mismatch.
Only the files the server needs are kept from the model archive (int8 encoder and joiner, the
decoder, the tokens): the archive also carries fp32 copies that would add 330 MB to the image.
"""

import hashlib
import sys
import tarfile
import urllib.request
from pathlib import Path

RELEASE = sys.argv[1]  # the sherpa-onnx release tag, e.g. v1.13.8
MODEL = sys.argv[2]  # the model archive's name without .tar.bz2
MODEL_SHA256 = sys.argv[3]
SERVER_SHA256 = sys.argv[4]  # python-api-examples/streaming_server.py at RELEASE
HTTP_SERVER_SHA256 = sys.argv[5]  # python-api-examples/http_server.py at RELEASE
APP = Path(sys.argv[6])  # where the scripts go
MODEL_DIR = Path(sys.argv[7])  # where the model files go

RAW = f"https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/{RELEASE}/python-api-examples"
ARCHIVE = f"https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/{MODEL}.tar.bz2"
KEEP = (
    "tokens.txt",
    "encoder-epoch-99-avg-1-chunk-16-left-128.int8.onnx",
    "decoder-epoch-99-avg-1-chunk-16-left-128.onnx",
    "joiner-epoch-99-avg-1-chunk-16-left-128.int8.onnx",
)
# http_server.py reads these files from the document root when the server starts, and the
# server refuses to start without them. The web page they belong to is not wanted here (the
# port is not published), so empty files are enough.
WEB_STUBS = (
    "css/bootstrap.min.css",
    "css/bootstrap.min.css.map",
    "index.html",
    "js/bootstrap.min.js",
    "js/bootstrap.min.js.map",
    "js/jquery-3.6.0.min.js",
    "js/offline_record.js",
    "js/popper.min.js",
    "js/popper.min.js.map",
    "js/streaming_record.js",
    "js/upload.js",
    "k2-logo.png",
    "nav-partial.html",
    "offline_record.html",
    "streaming_record.html",
    "upload.html",
)


def fetch(url: str) -> bytes:
    with urllib.request.urlopen(url, timeout=60) as response:
        return response.read()


def check(name: str, data: bytes, expected: str) -> None:
    actual = hashlib.sha256(data).hexdigest()
    if actual != expected:
        sys.exit(f"{name}: SHA-256 is {actual}, expected {expected}")


APP.mkdir(parents=True, exist_ok=True)
for name, expected in (("streaming_server.py", SERVER_SHA256), ("http_server.py", HTTP_SERVER_SHA256)):
    data = fetch(f"{RAW}/{name}")
    check(name, data, expected)
    (APP / name).write_bytes(data)

for stub in WEB_STUBS:
    path = APP / "web" / stub
    path.parent.mkdir(parents=True, exist_ok=True)
    path.touch()

# The archive is 310 MB: keep it on disk for the hash, read it back once, delete it.
archive = APP / "model.tar.bz2"
digest = hashlib.sha256()
with urllib.request.urlopen(ARCHIVE, timeout=60) as response, archive.open("wb") as out:
    while chunk := response.read(1 << 20):
        digest.update(chunk)
        out.write(chunk)
if digest.hexdigest() != MODEL_SHA256:
    sys.exit(f"{MODEL}.tar.bz2: SHA-256 is {digest.hexdigest()}, expected {MODEL_SHA256}")

MODEL_DIR.mkdir(parents=True, exist_ok=True)
found = set()
with tarfile.open(archive, "r:bz2") as tar:
    for member in tar:
        base = Path(member.name).name
        if member.isfile() and base in KEEP and Path(member.name).parent.name == MODEL:
            source = tar.extractfile(member)
            if source is None:
                sys.exit(f"cannot read {member.name}")
            (MODEL_DIR / base).write_bytes(source.read())
            found.add(base)
archive.unlink()
if found != set(KEEP):
    sys.exit(f"model files missing from the archive: {sorted(set(KEEP) - found)}")
print("model files:", ", ".join(f"{p.name} {p.stat().st_size}" for p in sorted(MODEL_DIR.iterdir())))
