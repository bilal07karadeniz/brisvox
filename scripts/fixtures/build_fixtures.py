#!/usr/bin/env python3
"""Build real-speech test fixtures with exact ground truth.

Source: Google FLEURS (CC BY 4.0, see fixtures/NOTICE and docs/FIXTURES_SOURCES.md), pinned to
one Hugging Face commit. Only the first MBs of each language's dev audio archive are read (no
170 MB downloads); the tar stream is parsed by hand, so a truncated last member is harmless.

For each fixture, clips are peak-normalized to -6 dBFS (FLEURS levels vary by more than 100x),
trimmed of edge silence (energy gate on 10 ms windows) and concatenated with known silence gaps,
so every speech segment's first and last sample are known exactly. Output (16 kHz PCM16 mono):

    fixtures/<name>.wav            audio
    fixtures/<name>.json           ground truth sidecar (read by brisvox_sim::fixture)
    fixtures/sources.lock          SHA-256 of every source clip of the small set (tracked)
    fixtures/large.sources.lock    same for the larger set (tracked; the set itself is not)

The lock files pin the selection: a rebuild must select exactly the locked clips with the locked
checksums, or it fails. Pass --update-lock to (re)create a lock deliberately.

Usage (from the repository root):
    python3 scripts/fixtures/build_fixtures.py            # small set, committed
    python3 scripts/fixtures/build_fixtures.py --large    # larger set into fixtures/large/

Standard library only. Network timeouts are per socket operation (60 s).
"""

import argparse
import array
import hashlib
import json
import os
import sys
import urllib.request
import wave
import zlib

REV = "70bb2e84b976b7e960aa89f1c648e09c59f894dd"
BASE = f"https://huggingface.co/datasets/google/fleurs/resolve/{REV}/data"
LANGS = {"en": "en_us", "tr": "tr_tr"}
RATE = 16_000
# Bytes read from the start of each language's dev archive (cached under target/).
PREFIX_BYTES = {"en": 16 * 1024 * 1024, "tr": 64 * 1024 * 1024}
MIN_SAMPLES = 2 * RATE  # clips between 2 s ...
MAX_SAMPLES = 12 * RATE  # ... and 12 s (Turkish FLEURS sentences are long)
WINDOW = RATE // 100  # 10 ms energy windows
# Longest pause allowed inside a clip, so that one clip = one speech region for a VAD with a
# 300 ms hangover (read speech often pauses for a breath; such clips are skipped).
MAX_INTERNAL_PAUSE = 250 * RATE // 1000
# Speech left after trimming must be substantial (a loud click can dominate the energy gate).
MIN_SPEECH = 3 * RATE // 2

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
OUT = os.path.join(ROOT, "fixtures")
CACHE = os.path.join(ROOT, "target", "fixture-cache")

# Fixture layouts: (name, [(lang, clip index within that language's selection)], gaps in ms).
# gaps[0] = leading silence, gaps[i] = silence after segment i (last = tail).
SMALL = [
    ("en_two_utterances", [("en", 0), ("en", 1)], [500, 1_500, 1_200]),
    ("tr_two_utterances", [("tr", 0), ("tr", 1)], [500, 1_500, 1_200]),
    ("en_tr_mixed", [("en", 2), ("tr", 2)], [400, 1_000, 1_200]),
]
LARGE = [
    (f"{lang}_long_{i}", [(lang, j) for j in range(i * 3, i * 3 + 3)], [500, 1_200, 2_000, 1_200])
    for lang in LANGS
    for i in range(2)
]


def write_atomic(path, data):
    """Writes `data` to `path` via a temporary file, so an interrupted run never leaves half a file."""
    tmp = path + ".tmp"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, path)


def fetch(url, limit=None):
    """GET `url`; with `limit`, read only that many bytes and drop the connection."""
    request = urllib.request.Request(url, headers={"User-Agent": "brisvox-fixtures"})
    with urllib.request.urlopen(request, timeout=60) as response:
        if not limit:
            return response.read()
        data = response.read(limit)
        length = response.headers.get("Content-Length")
        expected = min(limit, int(length)) if length and length.isdigit() else limit
        if len(data) < expected:
            raise SystemExit(f"short read from {url}: {len(data)} of {expected} bytes")
        return data


def cached(name, producer):
    os.makedirs(CACHE, exist_ok=True)
    path = os.path.join(CACHE, name)
    if not os.path.exists(path):
        write_atomic(path, producer())
    with open(path, "rb") as f:
        return f.read()


def tar_members(data):
    """Yields (basename, bytes) of complete regular files in a (possibly truncated) tar."""
    at = 0
    while at + 512 <= len(data):
        header = data[at : at + 512]
        if header == b"\0" * 512:
            return
        name = header[0:100].split(b"\0", 1)[0].decode("utf-8", "replace")
        try:
            size = int(header[124:136].split(b"\0", 1)[0].strip() or b"0", 8)
        except ValueError:
            raise SystemExit(f"corrupt tar header at byte {at}")
        kind = header[156:157]
        body = at + 512
        if body + size > len(data):
            return  # truncated last member
        if kind in (b"0", b"\0"):
            yield os.path.basename(name), data[body : body + size]
        at = body + ((size + 511) // 512) * 512


def gunzip_prefix(blob):
    """Decompresses a truncated gzip stream (everything up to the cut)."""
    try:
        return zlib.decompressobj(31).decompress(blob)
    except zlib.error as e:
        raise SystemExit(f"corrupt gzip prefix ({e}); delete target/fixture-cache and retry")


def read_pcm(wav_bytes):
    """PCM16 mono 16 kHz samples of a WAV file (PCM16 or float32; FLEURS ships float32)."""
    fmt = None
    at = 12
    while at + 8 <= len(wav_bytes):
        chunk, size = wav_bytes[at : at + 4], int.from_bytes(wav_bytes[at + 4 : at + 8], "little")
        body = wav_bytes[at + 8 : at + 8 + size]
        if chunk == b"fmt ":
            fmt = (
                int.from_bytes(body[0:2], "little"),  # 1 = PCM, 3 = IEEE float
                int.from_bytes(body[2:4], "little"),  # channels
                int.from_bytes(body[4:8], "little"),  # rate
                int.from_bytes(body[14:16], "little"),  # bits
            )
        elif chunk == b"data" and fmt:
            code, channels, rate, bits = fmt
            if channels != 1 or rate != RATE:
                break
            if code == 1 and bits == 16:
                samples = array.array("h")
                samples.frombytes(body[: len(body) // 2 * 2])
                if sys.byteorder == "big":
                    samples.byteswap()
                return samples
            if code == 3 and bits == 32:
                floats = array.array("f")
                floats.frombytes(body[: len(body) // 4 * 4])
                if sys.byteorder == "big":
                    floats.byteswap()
                return array.array(
                    "h", (max(-32768, min(32767, round(x * 32767))) for x in floats)
                )
            break
        at += 8 + size + (size % 2)
    raise SystemExit(f"unexpected FLEURS audio format {fmt} (expected 16 kHz mono PCM16/float32)")


def normalize(samples):
    """Peak-normalizes to -6 dBFS: FLEURS levels vary by more than 100x between clips."""
    peak = max((abs(s) for s in samples), default=0)
    if peak == 0:
        return samples
    gain = 16_384 / peak
    return array.array("h", (max(-32768, min(32767, round(s * gain))) for s in samples))


def active_windows(samples):
    """Indices of 10 ms windows above the energy gate."""
    windows = [samples[i : i + WINDOW] for i in range(0, len(samples) - WINDOW + 1, WINDOW)]
    energy = [sum(s * s for s in w) / len(w) for w in windows]
    if not energy:
        return []
    gate = max(300.0**2, 0.02 * max(energy))
    return [i for i, e in enumerate(energy) if e >= gate]


def trim(samples):
    """(start, end) sample range of speech (first to last 10 ms window above the energy gate),
    or None if no window passes the gate."""
    active = active_windows(samples)
    if not active:
        return None
    return active[0] * WINDOW, (active[-1] + 1) * WINDOW


def longest_pause(samples):
    """Longest run of silent windows between the first and last active window, in samples."""
    active = active_windows(samples)
    gaps = [(b - a - 1) * WINDOW for a, b in zip(active, active[1:])]
    return max(gaps, default=0)


def select_clips(lang, count):
    """The first `count` usable clips (2-12 s long, >= 1.5 s of speech, no long internal pause)
    of the language's dev set, in archive order."""
    code = LANGS[lang]
    tsv = cached(f"{code}_dev.tsv", lambda: fetch(f"{BASE}/{code}/dev.tsv")).decode("utf-8")
    meta = {}
    for line in tsv.splitlines():
        cols = line.split("\t")
        if len(cols) >= 7:
            meta[cols[1]] = {"id": cols[0], "text": cols[2], "samples": int(cols[5])}
    prefix = PREFIX_BYTES[lang]
    blob = cached(
        f"{code}_dev_prefix_{prefix}.tar.gz",
        lambda: fetch(f"{BASE}/{code}/audio/dev.tar.gz", prefix),
    )
    clips = []
    for name, wav_bytes in tar_members(gunzip_prefix(blob)):
        info = meta.get(name)
        if not info or not (MIN_SAMPLES <= info["samples"] <= MAX_SAMPLES):
            continue
        pcm = normalize(read_pcm(wav_bytes))
        span = trim(pcm)
        if span is None or longest_pause(pcm) > MAX_INTERNAL_PAUSE or span[1] - span[0] < MIN_SPEECH:
            continue
        clips.append(
            {
                "lang": lang,
                "file": name,
                "text": info["text"],
                "sha256": hashlib.sha256(wav_bytes).hexdigest(),
                "pcm": pcm,
                "span": span,
            }
        )
        if len(clips) == count:
            break
    if len(clips) < count:
        raise SystemExit(f"only {len(clips)} usable {lang} clips in the prefix; raise PREFIX_BYTES")
    return clips


def verify_lock(clips, lock_path, update):
    """The selection must equal the lock (same clips, same checksums) unless `update`."""
    selected = {
        f"fleurs:{LANGS[c['lang']]}:dev:{c['file']}": c["sha256"] for c in clips
    }
    if not update:
        if not os.path.exists(lock_path):
            raise SystemExit(f"{lock_path} missing: run with --update-lock to create it")
        with open(lock_path, encoding="utf-8") as f:
            locked = json.load(f)
        if selected != locked:
            changed = sorted(set(selected.items()) ^ set(locked.items()))
            raise SystemExit(f"selection differs from {lock_path}: {changed}")
        return
    os.makedirs(os.path.dirname(lock_path), exist_ok=True)
    text = json.dumps(dict(sorted(selected.items())), indent=2) + "\n"
    write_atomic(lock_path, text.encode("utf-8"))


def write_fixture(directory, name, parts, gaps):
    pcm = array.array("h")
    segments = []
    pcm.extend([0] * (gaps[0] * RATE // 1000))
    for i, clip in enumerate(parts):
        start, end = clip["span"]
        offset = len(pcm)
        pcm.extend(clip["pcm"][start:end])
        segments.append(
            {
                "start": offset,
                "end": offset + (end - start),
                "lang": clip["lang"],
                "text": clip["text"],
                "source": f"fleurs:{LANGS[clip['lang']]}:dev:{clip['file']}",
            }
        )
        pcm.extend([0] * (gaps[i + 1] * RATE // 1000))
    os.makedirs(directory, exist_ok=True)
    if sys.byteorder == "big":
        pcm.byteswap()
    wav_path = os.path.join(directory, f"{name}.wav")
    with wave.open(wav_path + ".tmp", "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(pcm.tobytes())
    os.replace(wav_path + ".tmp", wav_path)
    sidecar = {"name": name, "sample_rate": RATE, "samples": len(pcm), "segments": segments}
    text = json.dumps(sidecar, indent=2, ensure_ascii=False) + "\n"
    write_atomic(os.path.join(directory, f"{name}.json"), text.encode("utf-8"))
    return len(pcm)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--large", action="store_true", help="build the larger, uncommitted set")
    parser.add_argument("--update-lock", action="store_true", help="accept and record the selection")
    args = parser.parse_args()
    if args.large:
        layouts = LARGE
        directory = os.path.join(OUT, "large")
        lock_path = os.path.join(OUT, "large.sources.lock")
    else:
        layouts, directory, lock_path = SMALL, OUT, os.path.join(OUT, "sources.lock")
    needed = {}
    for _, parts, _ in layouts:
        for lang, index in parts:
            needed[lang] = max(needed.get(lang, 0), index + 1)
    clips = {lang: select_clips(lang, count) for lang, count in needed.items()}
    verify_lock([c for cs in clips.values() for c in cs], lock_path, args.update_lock)
    for name, parts, gaps in layouts:
        samples = write_fixture(directory, name, [clips[l][i] for l, i in parts], gaps)
        print(f"{name}: {samples / RATE:.2f} s, {len(parts)} segments")


if __name__ == "__main__":
    main()
