#!/usr/bin/env python3
"""Wire-level check of the fake providers with plain Python clients (no brisvox, no Pipecat).

Streams the fixture to the fake Deepgram STT in real time, talks to the fake Aura TTS and the fake
chat endpoint, and checks the behaviour the README promises: transcripts, endpointing, Finalize,
latencies (with generous tolerances: a loaded laptop is not a stopwatch), Flushed/Cleared and the
SSE stream. Needs `websockets` and `httpx`, which the Pipecat baseline venv has:

    ../../../pcvenv/bin/python check_wire.py --url https://127.0.0.1:8443 --ca /path/to/ca.pem

`--url` http:// or https:// (ws:// and wss:// follow). Exit status 0 when every check passes.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import ssl
import sys
import time
import wave
from pathlib import Path

import httpx
from websockets.asyncio.client import connect

FRAME_MS = 20
failures: list[str] = []


def check(ok: bool, what: str, detail: str = "") -> None:
    print(("ok    " if ok else "FAIL  ") + what + (f"  ({detail})" if detail else ""))
    if not ok:
        failures.append(what)


def load(fixtures_dir: Path, name: str):
    with open(fixtures_dir / f"{name}.json", encoding="utf-8") as f:
        sidecar = json.load(f)
    with wave.open(str(fixtures_dir / f"{name}.wav"), "rb") as w:
        assert w.getframerate() == 16000 and w.getnchannels() == 1 and w.getsampwidth() == 2
        pcm = w.readframes(w.getnframes())
    return sidecar, pcm


async def check_stt(url: str, ctx, sidecar, pcm: bytes) -> None:
    query = "model=nova-3&encoding=linear16&sample_rate=16000&channels=1&endpointing=300&interim_results=true&smart_format=true"
    seg = sidecar["segments"][0]
    end_s = seg["end"] / 16000
    frame = 16000 * FRAME_MS // 1000 * 2
    t0 = time.monotonic()
    results: list[tuple[float, dict]] = []
    async with connect(f"{url}/v1/listen?{query}", ssl=ctx, additional_headers={"Authorization": "Token fake"}) as ws:

        async def reader():
            async for message in ws:
                results.append((time.monotonic() - t0, json.loads(message)))

        task = asyncio.create_task(reader())
        # Real-time pacing up to 1.5 s after the first utterance, then silence.
        send_until = int((end_s + 1.5) * 1000 / FRAME_MS)
        for i in range(send_until):
            chunk = pcm[i * frame : (i + 1) * frame] or bytes(frame)
            await ws.send(chunk)
            await asyncio.sleep(max(0.0, t0 + (i + 1) * FRAME_MS / 1000 - time.monotonic()))
        await asyncio.sleep(0.3)
        interims = [r for _, r in results if not r["is_final"]]
        finals = [(t, r) for t, r in results if r["is_final"]]
        check(len(interims) >= 3, "STT sends interim results", f"{len(interims)}")
        check(all(r["type"] == "Results" for _, r in results), "STT messages are Results")
        sizes = [len(json.dumps(r)) for _, r in results]
        check(min(sizes) > 400, "STT messages have realistic sizes", f"{min(sizes)}-{max(sizes)} bytes")
        check(len(finals) == 1, "STT sends one final for the utterance", f"{len(finals)}")
        if finals:
            t, final = finals[0]
            text = final["channel"]["alternatives"][0]["transcript"]
            check(text == seg["text"], "STT final carries the fixture text")
            check(final["speech_final"] and not final["from_finalize"], "endpointing final: speech_final, not from_finalize")
            delay = t - end_s
            # 300 ms of silence rounded up to 32 ms windows (320 ms) + 150 ms
            check(0.40 <= delay <= 0.80, "STT final comes ~470 ms after the end of speech", f"{delay * 1000:.0f} ms")
        # Finalize with nothing pending: an empty from_finalize result at once.
        results.clear()
        await ws.send(json.dumps({"type": "Finalize"}))
        await asyncio.sleep(0.3)
        empty = [r for _, r in results if r.get("from_finalize")]
        check(len(empty) == 1 and empty[0]["channel"]["alternatives"][0]["transcript"] == "", "Finalize with nothing pending: empty from_finalize")
        await ws.send(json.dumps({"type": "CloseStream"}))
        await asyncio.sleep(0.3)
        check(any(r.get("type") == "Metadata" for _, r in results), "CloseStream: Metadata")
        task.cancel()


async def check_stt_finalize(url: str, ctx, sidecar, pcm: bytes) -> None:
    """Finalize 100 ms into the silence after the first utterance: the final follows in ~150 ms."""
    query = "model=nova-3&encoding=linear16&sample_rate=16000&endpointing=300&interim_results=true"
    seg = sidecar["segments"][0]
    end_s = seg["end"] / 16000
    frame = 16000 * FRAME_MS // 1000 * 2
    t0 = time.monotonic()
    finals: list[tuple[float, dict]] = []
    async with connect(f"{url}/v1/listen?{query}", ssl=ctx) as ws:

        async def reader():
            async for message in ws:
                r = json.loads(message)
                if r["is_final"]:
                    finals.append((time.monotonic() - t0, r))

        task = asyncio.create_task(reader())
        sent_finalize = None
        for i in range(int((end_s + 1.0) * 1000 / FRAME_MS)):
            await ws.send(pcm[i * frame : (i + 1) * frame] or bytes(frame))
            if sent_finalize is None and (i + 1) * FRAME_MS / 1000 >= end_s + 0.12:
                await ws.send(json.dumps({"type": "Finalize"}))
                sent_finalize = time.monotonic() - t0
            await asyncio.sleep(max(0.0, t0 + (i + 1) * FRAME_MS / 1000 - time.monotonic()))
        await asyncio.sleep(0.2)
        task.cancel()
    check(len(finals) == 1, "Finalize before the endpoint: exactly one final", f"{len(finals)}")
    if finals and sent_finalize is not None:
        t, r = finals[0]
        check(r["from_finalize"] and r["is_final"], "Finalize final: from_finalize")
        check(r["channel"]["alternatives"][0]["transcript"] == seg["text"], "Finalize final carries the fixture text")
        check(0.10 <= t - sent_finalize <= 0.40, "Finalize final comes ~150 ms after Finalize", f"{(t - sent_finalize) * 1000:.0f} ms")


async def check_tts(url: str, ctx) -> None:
    async with connect(f"{url}/v1/speak?model=aura-2-thalia-en&encoding=linear16&sample_rate=24000", ssl=ctx, additional_headers={"Authorization": "Token fake"}) as ws:
        first = json.loads(await ws.recv())
        check(first["type"] == "Metadata" and first["model_name"] == "aura-2-thalia-en", "TTS: Metadata on open")
        t0 = time.monotonic()
        await ws.send(json.dumps({"type": "Speak", "text": "Okay, got it."}))
        await ws.send(json.dumps({"type": "Flush"}))
        first_audio = None
        audio = 0
        flushed_at = None
        while True:
            m = await asyncio.wait_for(ws.recv(), 5)
            now = time.monotonic() - t0
            if isinstance(m, bytes):
                first_audio = first_audio if first_audio is not None else now
                audio += len(m)
            else:
                msg = json.loads(m)
                if msg["type"] == "Flushed":
                    flushed_at = now
                    break
        check(first_audio is not None and 0.12 <= first_audio <= 0.35, "TTS first audio ~150 ms", f"{(first_audio or 0) * 1000:.0f} ms")
        # 13 characters * 60 ms = 780 ms of 24 kHz linear16 in 40 ms frames (20 frames of 1920 bytes)
        check(audio == 20 * 1920, "TTS audio is 60 ms per character", f"{audio} bytes")
        check(flushed_at is not None and 0.45 <= flushed_at <= 0.9, "TTS Flushed after the audio (2x real time)", f"{(flushed_at or 0) * 1000:.0f} ms")
        # Clear in the middle of a long text
        await ws.send(json.dumps({"type": "Speak", "text": "A much longer sentence that will take a while to say aloud."}))
        await asyncio.sleep(0.4)
        await ws.send(json.dumps({"type": "Clear"}))
        got_cleared = False
        extra = 0
        deadline = time.monotonic() + 1.5
        while time.monotonic() < deadline:
            try:
                m = await asyncio.wait_for(ws.recv(), 0.3)
            except asyncio.TimeoutError:
                break
            if isinstance(m, bytes):
                extra += 1
            elif json.loads(m)["type"] == "Cleared":
                got_cleared = True
        check(got_cleared, "TTS Clear: Cleared")
        await ws.send(json.dumps({"type": "Close"}))


async def check_llm(base: str, verify, fixtures_sidecar) -> None:
    text = fixtures_sidecar["segments"][0]["text"]
    body = {
        "model": "fake-model",
        "stream": True,
        "stream_options": {"include_usage": True},
        "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": text}],
    }
    async with httpx.AsyncClient(verify=verify, http2=False) as client:
        t0 = time.monotonic()
        pieces: list[tuple[float, str]] = []
        done = False
        usage = False
        async with client.stream("POST", f"{base}/v1/chat/completions", json=body, headers={"Authorization": "Bearer fake"}) as r:
            check(r.status_code == 200 and r.headers["content-type"].startswith("text/event-stream"), "LLM: SSE response")
            async for line in r.aiter_lines():
                if not line.startswith("data: "):
                    continue
                payload = line[6:]
                if payload == "[DONE]":
                    done = True
                    break
                chunk = json.loads(payload)
                usage = usage or bool(chunk.get("usage"))
                if chunk["choices"] and chunk["choices"][0]["delta"].get("content"):
                    pieces.append((time.monotonic() - t0, chunk["choices"][0]["delta"]["content"]))
        check("".join(p for _, p in pieces) == "Okay, got it.", "LLM: the reply text", repr("".join(p for _, p in pieces)))
        check(len(pieces) == 5, "LLM: a token per word or punctuation mark", f"{len(pieces)}")
        check(pieces and 0.3 <= pieces[0][0] <= 0.6, "LLM: first token ~350 ms", f"{(pieces[0][0] if pieces else 0) * 1000:.0f} ms")
        check(done and usage, "LLM: usage chunk and [DONE]")
        # not streamed
        r = await client.post(f"{base}/v1/chat/completions", json={**body, "stream": False}, headers={"Authorization": "Bearer fake"})
        check(r.status_code == 200 and r.json()["choices"][0]["message"]["content"] == "Okay, got it.", "LLM: non-streamed completion")


async def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--url", default="https://127.0.0.1:8443", help="http(s)://host:port of the fake server")
    ap.add_argument("--ca", help="CA certificate PEM (https/wss)")
    ap.add_argument("--fixtures-dir", default=str(Path(__file__).resolve().parents[2] / "fixtures"))
    ap.add_argument("--fixture", default="en_two_utterances")
    args = ap.parse_args()
    secure = args.url.startswith("https://")
    ctx = ssl.create_default_context(cafile=args.ca) if secure else None
    ws_url = ("wss://" if secure else "ws://") + args.url.split("://", 1)[1]
    sidecar, pcm = load(Path(args.fixtures_dir), args.fixture)
    await check_llm(args.url, ctx if secure else False, sidecar)
    await check_tts(ws_url, ctx)
    await check_stt(ws_url, ctx, sidecar, pcm)
    await check_stt_finalize(ws_url, ctx, sidecar, pcm)
    print(f"\n{len(failures)} failed" if failures else "\nall checks passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
