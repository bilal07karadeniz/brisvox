#!/usr/bin/env python3
"""The capacity load generator for LiveKit Agents: N room participants replay a fixture in real time.

This is the LiveKit counterpart of ``brisvox-bench capacity`` (crates/brisvox-bench): the same
fixture (``fixtures/en_two_utterances``, gaps padded to 4 s), the same ramp (steps, bisection,
two calls per slot, starts spread over 4 s, 3 s cooldown, one warm-up call), the same V2V
definition (the first audible bot audio frame after the ground-truth end of the user's speech,
minus that end of speech; ``Tracker`` below is a port of ``v2v/measure.rs``), the same turn
outcomes (answered / talked over / bot speaking / no response), the same SLO (p95 V2V <= floor +
250 ms, no failed call, no lost turn) and the same report: ``brisvox-bench/capacity/2`` JSON plus
the markdown table, so ``bench/capacity_summary.py`` and the Rust schema test read the files.

What a "call" is here: one room per call. The caller joins a fresh room with a LiveKit token
(``livekit-api``), publishes a microphone track (``rtc.AudioSource``, 16 kHz mono PCM, fed with the
fixture's 20 ms frames at real time), and subscribes to the agent's audio. The LiveKit server
dispatches an agent job to the registered worker (automatic dispatch). The stream origin
``begin`` is the moment the agent is ready: the agent participant is in the room, its audio
track is subscribed and it has published ``lk.agent.state`` (the analogue of the websocket
protocol's ``started``). Bot audio is stamped when the frame reaches the event loop of the
caller process (``rtc.AudioStream``, 16 kHz mono, 20 ms frames).

The load generator runs as ``--workers`` processes (one asyncio loop each, slots dealt out round
robin): a Python event loop paces about 50 frames per second per call and would otherwise limit
the step before the server does. Send lateness (frame handed to the SDK later than its deadline)
is measured and judged exactly as in the Rust generator (generator limited when more than 1 % of
the frames are more than 100 ms late).

CPU and memory come from ``sampler.py`` (cgroups of the agent container, the LiveKit server
container and this container), not from a process tree: see the README for why.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import multiprocessing as mp
import os
import platform
import subprocess
import sys
import time
import uuid
import wave
from dataclasses import dataclass
from pathlib import Path

import numpy as np

FRAME_MS = 20
SILENCE_PEAK = 200  # peak amplitude (of 32768) below which a bot frame is silence
OVERLAP_GUARD_S = 0.100
READY_STATES = {"listening", "idle", "thinking", "speaking"}
SCHEMA = "brisvox-bench/capacity/2"

# ---------------------------------------------------------------------------------------------
# Fixture (port of brisvox_sim::fixture and fixture_load::pad_gaps)


@dataclass
class Segment:
    start: int
    end: int
    lang: str
    text: str


@dataclass
class Fixture:
    name: str
    rate: int
    pcm: np.ndarray  # int16
    segments: list[Segment]

    def time_of(self, sample: int) -> float:
        return sample / self.rate

    @property
    def duration(self) -> float:
        return self.pcm.size / self.rate


def load_fixture(directory: Path, name: str) -> Fixture:
    with wave.open(str(directory / f"{name}.wav"), "rb") as w:
        assert w.getnchannels() == 1 and w.getsampwidth() == 2, "fixtures are mono PCM16"
        rate = w.getframerate()
        pcm = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").copy()
    sidecar = json.loads((directory / f"{name}.json").read_text(encoding="utf-8"))
    segments = [Segment(s["start"], s["end"], s.get("lang", "en"), s["text"]) for s in sidecar["segments"]]
    return Fixture(name, rate, pcm, segments)


def pad_gaps(fx: Fixture, min_gap_s: float) -> Fixture:
    """Every silence between two utterances at least ``min_gap_s`` long (zeros inserted)."""
    min_gap = int(min_gap_s * 1_000_000) * fx.rate // 1_000_000
    parts: list[np.ndarray] = []
    segments: list[Segment] = []
    copied = 0
    written = 0
    previous_end: int | None = None
    for seg in fx.segments:
        parts.append(fx.pcm[copied : seg.start])
        written += seg.start - copied
        if previous_end is not None:
            extra = max(0, min_gap - (seg.start - previous_end))
            parts.append(np.zeros(extra, dtype="<i2"))
            written += extra
        start = written
        parts.append(fx.pcm[seg.start : seg.end])
        written += seg.end - seg.start
        copied = seg.end
        previous_end = seg.end
        segments.append(Segment(start, written, seg.lang, seg.text))
    parts.append(fx.pcm[copied:])
    return Fixture(fx.name, fx.rate, np.concatenate(parts), segments)


# ---------------------------------------------------------------------------------------------
# Measurement (port of v2v/measure.rs)


class Tracker:
    """Online V2V: per turn, whether audio overlapped its end of speech and the first audible
    arrival after it."""

    def __init__(self, fx: Fixture) -> None:
        self.turns = [(i, s.lang, fx.time_of(s.start), fx.time_of(s.end)) for i, s in enumerate(fx.segments)]
        self.overlap = [False] * len(self.turns)
        self.first_after: list[float | None] = [None] * len(self.turns)

    def observe(self, at: float) -> None:
        for i, (_, _, _, eos) in enumerate(self.turns):
            if at > eos:
                cur = self.first_after[i]
                self.first_after[i] = at if cur is None else min(cur, at)
            elif at > eos - OVERLAP_GUARD_S:
                self.overlap[i] = True

    def results(self) -> list[dict]:
        out = []
        for i, (index, lang, _start, eos) in enumerate(self.turns):
            nxt = self.turns[i + 1][2] if i + 1 < len(self.turns) else None
            if self.overlap[i]:
                out.append({"index": index, "outcome": "bot_speaking", "v2v_us": None})
                continue
            first = self.first_after[i]
            if first is None:
                out.append({"index": index, "outcome": "no_response", "v2v_us": None})
                continue
            latency_us = int(round((first - eos) * 1_000_000))
            outcome = "talked_over" if (nxt is not None and first >= nxt) else "answered"
            out.append({"index": index, "outcome": outcome, "v2v_us": latency_us})
        return out


def nearest_rank(sorted_samples: list[int], permille: int) -> int:
    rank = -(-permille * len(sorted_samples) // 1000)  # ceil
    return sorted_samples[max(rank - 1, 0)]


def dist(samples: list[int]) -> dict | None:
    if not samples:
        return None
    s = sorted(samples)
    return {
        "count": len(s),
        "p50_us": nearest_rank(s, 500),
        "p90_us": nearest_rank(s, 900),
        "p95_us": nearest_rank(s, 950),
        "p99_us": nearest_rank(s, 990),
        "max_us": s[-1],
    }


# ---------------------------------------------------------------------------------------------
# One call (in a worker process)


@dataclass
class CallConfig:
    url: str
    api_key: str
    api_secret: str
    fixture: Fixture
    response_timeout: float
    connect_timeout: float
    ready_timeout: float
    lateness_limit: float
    agent_name: str = ""


def make_token(cfg: CallConfig, room: str, identity: str) -> str:
    from livekit import api

    grants = api.VideoGrants(room_join=True, room=room, can_publish=True, can_subscribe=True)
    return api.AccessToken(cfg.api_key, cfg.api_secret).with_identity(identity).with_grants(grants).to_jwt()


async def sleep_until(deadline: float) -> None:
    delay = deadline - time.monotonic()
    if delay > 0:
        await asyncio.sleep(delay)


async def run_call(cfg: CallConfig) -> dict:
    """One call. Returns {failure, turns, max_late_us, fixture_frames, late_frames, ...}."""
    from livekit import rtc

    fx = cfg.fixture
    frame_samples = fx.rate * FRAME_MS // 1000
    total = fx.pcm.size
    frames = -(-total // frame_samples)
    last_eos = fx.time_of(fx.segments[-1].end)
    result: dict = {"failure": None, "turns": [], "max_late_us": 0, "fixture_frames": 0, "late_frames": 0}
    tracker = Tracker(fx)
    st = {"begin": None, "answered_last": False, "disconnected": None, "agent_left": False, "audio_frames": 0, "audible": 0}
    tasks: list[asyncio.Task] = []
    room = rtc.Room()
    source = None
    room_name = f"cap-{uuid.uuid4().hex[:12]}"

    async def read_audio(track: rtc.Track) -> None:
        stream = rtc.AudioStream(track, sample_rate=fx.rate, num_channels=1, frame_size_ms=FRAME_MS)
        try:
            async for ev in stream:
                now = time.monotonic()  # stamped before anything else is done with the frame
                begin = st["begin"]
                st["audio_frames"] += 1
                if begin is None:
                    continue
                data = np.frombuffer(ev.frame.data, dtype=np.int16)
                if data.size and int(np.abs(data.astype(np.int32)).max()) >= SILENCE_PEAK:
                    st["audible"] += 1
                    offset = now - begin
                    tracker.observe(offset)
                    if offset > last_eos:
                        st["answered_last"] = True
        except asyncio.CancelledError:
            raise
        except Exception as e:  # noqa: BLE001
            st["reader_error"] = repr(e)
        finally:
            await stream.aclose()

    @room.on("track_subscribed")
    def on_track(track: rtc.Track, _pub, participant: rtc.RemoteParticipant) -> None:
        if track.kind == rtc.TrackKind.KIND_AUDIO and participant.kind == rtc.ParticipantKind.PARTICIPANT_KIND_AGENT:
            tasks.append(asyncio.create_task(read_audio(track)))

    @room.on("participant_disconnected")
    def on_left(participant: rtc.RemoteParticipant) -> None:
        if participant.kind == rtc.ParticipantKind.PARTICIPANT_KIND_AGENT:
            st["agent_left"] = True

    @room.on("disconnected")
    def on_disconnected(reason) -> None:
        st["disconnected"] = str(reason)

    try:
        try:
            token = make_token(cfg, room_name, f"caller-{room_name}")
            await asyncio.wait_for(room.connect(cfg.url, token, rtc.RoomOptions(auto_subscribe=True)), cfg.connect_timeout)
        except asyncio.TimeoutError:
            result["failure"] = "connect timed out"
            return result
        except Exception as e:  # noqa: BLE001
            result["failure"] = f"connect: {e}"
            return result
        source = rtc.AudioSource(fx.rate, 1)
        track = rtc.LocalAudioTrack.create_audio_track("mic", source)
        try:
            await asyncio.wait_for(
                room.local_participant.publish_track(track, rtc.TrackPublishOptions(source=rtc.TrackSource.SOURCE_MICROPHONE)),
                cfg.connect_timeout,
            )
        except Exception as e:  # noqa: BLE001
            result["failure"] = f"publish: {e!r}"
            return result

        t_joined = time.monotonic()
        # The analogue of `started`: the agent is in the room, its audio is subscribed and it
        # reports a state other than "initializing".
        ready_deadline = time.monotonic() + cfg.ready_timeout
        while True:
            if st["disconnected"]:
                result["failure"] = f"the connection broke before the agent was ready: {st['disconnected']}"
                return result
            agent = next(
                (p for p in room.remote_participants.values() if p.kind == rtc.ParticipantKind.PARTICIPANT_KIND_AGENT), None
            )
            if agent is not None and agent.attributes.get("lk.agent.state") in READY_STATES and tasks:
                break
            if time.monotonic() > ready_deadline:
                result["failure"] = f"no answer to start: no agent ready after {cfg.ready_timeout:g} s"
                return result
            await asyncio.sleep(0.02)

        begin = time.monotonic()
        st["begin"] = begin
        result["ready_s"] = round(begin - t_joined, 3)
        if os.environ.get("LK_TRACE") == "1":
            eos = ",".join(f"{time.time() + fx.time_of(sg.end):.4f}" for sg in fx.segments)
            print(f"TRACE {room_name} {time.time():.4f} begin eos_epochs={eos}", flush=True)
        tail_deadline = begin + fx.duration + cfg.response_timeout
        pcm_bytes = fx.pcm.tobytes()
        silence = bytes(frame_samples * 2)
        sent = 0
        padded = 0
        max_late = 0.0
        late_frames = 0
        while True:
            in_fixture = sent < frames
            if not in_fixture and st["answered_last"]:
                break
            if in_fixture:
                end_sample = min((sent + 1) * frame_samples, total)
            else:
                end_sample = (padded + 1) * frame_samples + total
            due = begin + fx.time_of(end_sample)
            if not in_fixture and due > tail_deadline:
                break
            if st["disconnected"] or st["agent_left"]:
                break
            await sleep_until(due)
            if in_fixture:
                late = max(0.0, time.monotonic() - due)
                max_late = max(max_late, late)
                if late > cfg.lateness_limit:
                    late_frames += 1
                chunk = pcm_bytes[sent * frame_samples * 2 : end_sample * 2]
            else:
                chunk = silence
            frame = rtc.AudioFrame(chunk, fx.rate, 1, len(chunk) // 2)
            try:
                await asyncio.wait_for(source.capture_frame(frame), 5.0)
            except Exception as e:  # noqa: BLE001
                result["failure"] = f"a write failed: {e!r}"
                break
            if in_fixture:
                sent += 1
            else:
                padded += 1
        result["max_late_us"] = int(max_late * 1_000_000)
        result["fixture_frames"] = sent
        result["late_frames"] = late_frames
        if result["failure"] is None:
            if st["agent_left"]:
                result["failure"] = "the agent left the room during the call"
            elif st["disconnected"]:
                result["failure"] = f"the connection broke: {st['disconnected']}"
            elif sent < frames:
                result["failure"] = "the call ended before all input was sent"
        result["turns"] = tracker.results()
        result["audio_frames"] = st["audio_frames"]
        result["audible_frames"] = st["audible"]
        return result
    finally:
        for t in tasks:
            t.cancel()
        for t in tasks:
            try:
                await t
            except BaseException:  # noqa: BLE001
                pass
        try:
            await asyncio.wait_for(room.disconnect(), 5.0)
        except BaseException:  # noqa: BLE001
            pass
        if source is not None:
            try:
                await source.aclose()
            except BaseException:  # noqa: BLE001
                pass


async def play_slot(cfg: CallConfig, start_at: float, rounds: int, pause: float) -> list[dict]:
    await sleep_until(start_at)
    calls = []
    for r in range(rounds):
        try:
            calls.append(await run_call(cfg))
        except Exception as e:  # noqa: BLE001
            calls.append({"failure": f"call task: {e!r}", "turns": [], "max_late_us": 0, "fixture_frames": 0, "late_frames": 0})
        if r + 1 < rounds:
            await asyncio.sleep(pause)
    return calls


def worker_main(conn, base: dict) -> None:
    """A worker process: runs the slots the coordinator deals it, one command at a time."""
    from livekit import rtc  # noqa: F401  (loads the native SDK once per process)

    fixture = load_fixture(Path(base["fixtures_dir"]), base["fixture"])
    fixture = pad_gaps(fixture, base["min_gap"])

    async def main() -> None:
        loop = asyncio.get_running_loop()
        # Initialise the SDK's FFI before the first measured call (an AudioSource needs it).
        warm = rtc.AudioSource(fixture.rate, 1)
        await warm.aclose()
        conn.send({"ready": True})
        while True:
            cmd = await loop.run_in_executor(None, conn.recv)
            if cmd["op"] == "quit":
                return
            cfg = CallConfig(
                url=base["url"],
                api_key=base["api_key"],
                api_secret=base["api_secret"],
                fixture=fixture,
                response_timeout=base["response_timeout"],
                connect_timeout=base["connect_timeout"],
                ready_timeout=base["ready_timeout"],
                lateness_limit=base["lateness_limit"],
            )
            slots = await asyncio.gather(*[play_slot(cfg, start_at, cmd["rounds"], cmd["pause"]) for start_at in cmd["starts"]])
            conn.send({"calls": [c for slot in slots for c in slot]})

    asyncio.run(main())


# ---------------------------------------------------------------------------------------------
# Judging a step (port of capacity::summarize and report::conclude)


@dataclass
class Judge:
    limit_us: int
    floor_ms: int
    over_floor_ms: int
    max_bad_rate: float
    lateness_limit_ms: int
    late_share_limit: float


MAX_FAILURE_NOTES = 5


def summarize(concurrent: int, turns_per_call: int, calls: list[dict], judge: Judge) -> dict:
    failed = [c for c in calls if c["failure"]]
    failures: list[str] = []
    for c in failed:
        if len(failures) < MAX_FAILURE_NOTES and c["failure"] not in failures:
            failures.append(c["failure"])
    answered = talked = speaking = none = 0
    v2v: list[int] = []
    for c in calls:
        if c["failure"]:
            continue
        for t in c["turns"]:
            o = t["outcome"]
            if o == "answered":
                answered += 1
                v2v.append(t["v2v_us"])
            elif o == "talked_over":
                talked += 1
            elif o == "bot_speaking":
                speaking += 1
            else:
                none += 1
    expected = len(calls) * turns_per_call
    bad = max(expected - answered, 0)
    bad_rate = bad / expected if expected else 0.0
    max_late = max((c["max_late_us"] for c in calls), default=0)
    fixture_frames = sum(c["fixture_frames"] for c in calls)
    late_frames = sum(c["late_frames"] for c in calls)
    late_share = late_frames / fixture_frames if fixture_frames else 0.0
    d = dist(v2v)
    over = d["p95_us"] - judge.floor_ms * 1000 if d else None
    verdict = "pass"
    reasons: list[str] = []

    def fail(reason: str) -> None:
        nonlocal verdict
        if verdict == "pass":
            verdict = "fail"
        reasons.append(reason)

    if not calls:
        fail("no call was played")
    if failed:
        fail(f"{len(failed)} of {len(calls)} calls failed")
    if bad_rate > judge.max_bad_rate:
        fail(
            f"{bad} of {expected} turns lost or not clean ({talked} talked over, {none} no response, "
            f"{speaking} bot speaking; allowed {judge.max_bad_rate * 100:.1f} %)"
        )
    if d is None and calls:
        fail("no turn was answered")
    elif d is not None and d["p95_us"] > judge.limit_us:
        fail(
            f"p95 V2V {d['p95_us'] // 1000} ms is above the limit {judge.limit_us // 1000} ms "
            f"(floor {judge.floor_ms} ms + {judge.over_floor_ms} ms)"
        )
    if late_share > judge.late_share_limit:
        note = (
            f"{late_frames} of {fixture_frames} frames ({late_share * 100:.1f} %) went out more than "
            f"{judge.lateness_limit_ms} ms late (limit {judge.late_share_limit * 100:.1f} %, latest {max_late // 1000} ms)"
        )
        if verdict == "pass":
            verdict = "generator_limited"
            reasons.append(f"{note}: the load generator is the limit")
        else:
            reasons.append(f"{note}: the generator may have contributed, check its CPU column")
    return {
        "concurrent": concurrent,
        "calls": len(calls),
        "calls_failed": len(failed),
        "failures": failures,
        "turns_expected": expected,
        "turns_answered": answered,
        "talked_over": talked,
        "bot_speaking": speaking,
        "no_response": none,
        "bad_rate": bad_rate,
        "v2v": d,
        "p95_over_floor_us": over,
        "max_send_lateness_us": max_late,
        "late_frames": late_frames,
        "timed_frames": fixture_frames,
        "wall_ms": 0,
        "rss_before_bytes": None,
        "target": None,
        "client": None,
        "verdict": verdict,
        "reasons": reasons,
    }


def conclude(steps: list[dict]) -> dict:
    stops = [s for s in steps if s["verdict"] != "pass"]
    stop = min(stops, key=lambda s: s["concurrent"]) if stops else None
    fails = [s["concurrent"] for s in steps if s["verdict"] == "fail"]
    first_failure = min(fails) if fails else None
    passing = [s["concurrent"] for s in steps if s["verdict"] == "pass" and (stop is None or s["concurrent"] < stop["concurrent"])]
    max_concurrent = max(passing) if passing else None
    if stop is None:
        limit = "not_reached"
    elif stop["verdict"] == "generator_limited":
        limit = "generator_limited"
    elif max_concurrent is None:
        limit = "first_step_failed"
    else:
        limit = "found"
    return {"max_concurrent": max_concurrent, "first_failure": first_failure, "limit": limit}


class Ramp:
    def __init__(self, steps: list[int], resolution: int, keep_going: bool) -> None:
        self.pending = list(steps)
        self.results: list[tuple[int, str]] = []
        self.resolution = resolution
        self.keep_going = keep_going

    def next_step(self) -> int | None:
        if self.pending:
            return self.pending.pop(0)
        if self.resolution == 0:
            return None
        stops = [n for n, v in self.results if v != "pass"]
        if not stops:
            return None
        stop = min(stops)
        passes = [n for n, v in self.results if v == "pass" and n < stop]
        if not passes:
            return None
        pas = max(passes)
        if stop - pas <= self.resolution:
            return None
        mid = pas + (stop - pas) // 2
        if pas < mid < stop and not any(n == mid for n, _ in self.results):
            return mid
        return None

    def record(self, n: int, verdict: str) -> None:
        self.results.append((n, verdict))
        if verdict != "pass" and not self.keep_going:
            self.pending.clear()


# ---------------------------------------------------------------------------------------------
# Usage sampling (reads the sampler's file)


class Samples:
    def __init__(self, path: Path) -> None:
        self.path = path
        self.offset = 0
        self.by_name: dict[str, list[dict]] = {}

    def refresh(self) -> None:
        try:
            with open(self.path, "rb") as f:
                f.seek(self.offset)
                data = f.read()
        except OSError:
            return
        end = data.rfind(b"\n")
        if end < 0:
            return
        self.offset += end + 1
        for line in data[: end + 1].splitlines():
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            self.by_name.setdefault(rec["name"], []).append(rec)

    def latest(self, name: str) -> dict | None:
        rows = self.by_name.get(name)
        return rows[-1] if rows else None

    def usage(self, name: str, t0: float, t1: float) -> dict | None:
        rows = [r for r in self.by_name.get(name, []) if t0 <= r["t"] <= t1 and r["cpu_usec"] is not None]
        if not rows:
            return None
        cpu = None
        if len(rows) >= 2 and rows[-1]["t"] > rows[0]["t"]:
            cpu = (rows[-1]["cpu_usec"] - rows[0]["cpu_usec"]) / 1e6 / (rows[-1]["t"] - rows[0]["t"])
        return {
            "cpu_cores": cpu,
            # The schema's resident-set column: PSS (shared pages divided among the processes
            # that share them). A process-per-job worker forks its jobs from a preloaded server,
            # so the plain sum of VmRSS counts the shared model pages once per process (5.6 GB
            # for 10 calls in a 4 GiB container); for processes that share nothing (Pipecat's
            # workers, brisvox) the two sums are the same.
            "rss_peak_bytes": max(r["pss"] for r in rows),
            "processes": max(r["procs"] for r in rows),
            "readings": len(rows),
            # not in the Rust schema (extra fields are ignored by it):
            "rss_sum_peak_bytes": max(r["rss"] for r in rows),
            "cgroup_memory_peak_bytes": max((r["mem_current"] or 0) for r in rows),
        }


# ---------------------------------------------------------------------------------------------
# The coordinator


class Coordinator:
    def __init__(self, args: argparse.Namespace, samples: Samples) -> None:
        self.args = args
        self.samples = samples
        self.ctx = mp.get_context("spawn")
        self.workers: list[tuple] = []
        fixture = pad_gaps(load_fixture(Path(args.fixtures_dir), args.fixture), args.min_gap_ms / 1000)
        self.turns_per_call = len(fixture.segments)
        self.fixture = fixture
        self.judge = Judge(
            limit_us=(args.floor_ms + args.slo_over_floor_ms) * 1000,
            floor_ms=args.floor_ms,
            over_floor_ms=args.slo_over_floor_ms,
            max_bad_rate=args.max_bad_rate,
            lateness_limit_ms=args.lateness_limit_ms,
            late_share_limit=args.late_share_limit,
        )
        self.base = {
            "url": args.url,
            "api_key": args.api_key,
            "api_secret": args.api_secret,
            "fixtures_dir": args.fixtures_dir,
            "fixture": args.fixture,
            "min_gap": args.min_gap_ms / 1000,
            "response_timeout": args.response_timeout_ms / 1000,
            "connect_timeout": args.connect_timeout_ms / 1000,
            "ready_timeout": args.ready_timeout_ms / 1000,
            "lateness_limit": args.lateness_limit_ms / 1000,
        }

    def start_workers(self) -> None:
        for _ in range(self.args.workers):
            parent, child = self.ctx.Pipe()
            proc = self.ctx.Process(target=worker_main, args=(child, self.base), daemon=True)
            proc.start()
            self.workers.append((proc, parent))
        for _, conn in self.workers:
            assert conn.recv().get("ready")

    def stop_workers(self) -> None:
        for proc, conn in self.workers:
            try:
                conn.send({"op": "quit"})
            except OSError:
                pass
        for proc, _ in self.workers:
            proc.join(timeout=10)
            if proc.is_alive():
                proc.terminate()

    def run_step(self, n: int, rounds: int) -> dict:
        a = self.args
        self.samples.refresh()
        before = self.samples.latest("agent")
        rss_before = before["pss"] if before else None
        t0 = time.monotonic()
        base = t0 + 1.0  # time to deliver the commands
        starts = [base + (a.spread_ms / 1000) * i / n for i in range(n)]
        per_worker: list[list[float]] = [[] for _ in self.workers]
        for i, s in enumerate(starts):
            per_worker[i % len(self.workers)].append(s)
        for (_, conn), mine in zip(self.workers, per_worker):
            conn.send({"op": "step", "starts": mine, "rounds": rounds, "pause": a.pause_ms / 1000})
        calls: list[dict] = []
        for (_, conn), mine in zip(self.workers, per_worker):
            reply = conn.recv()
            calls.extend(reply["calls"])
        t1 = time.monotonic()
        time.sleep(a.sample_period * 1.5)  # the sampler's last readings of the step
        self.samples.refresh()
        report = summarize(n, self.turns_per_call, calls, self.judge)
        if self.args.dump_turns:
            with open(self.args.dump_turns, "a") as f:
                for c in calls:
                    f.write(json.dumps({"step": n, "failure": c["failure"], "ready_s": c.get("ready_s"), "turns": c["turns"]}) + "\n")
        report["wall_ms"] = int((t1 - t0) * 1000)
        report["rss_before_bytes"] = rss_before
        report["target"] = self.samples.usage("agent", t0, t1)
        report["client"] = self.samples.usage("client", t0, t1)
        report["sfu"] = self.samples.usage("sfu", t0, t1)  # extra: the LiveKit server container
        return report

    def run(self) -> dict:
        a = self.args
        steps = sorted(set(a.steps))
        ramp = Ramp(steps, a.resolution, a.keep_going)
        reports: list[dict] = []
        warmup = None
        self.start_workers()
        try:
            if a.warmup_calls > 0:
                warmup = self.run_step(1, a.warmup_calls)
                self.progress(warmup, True)
            while (n := ramp.next_step()) is not None:
                if reports or warmup:
                    time.sleep(a.cooldown_ms / 1000)
                    self.settle()
                report = self.run_step(n, a.rounds)
                self.progress(report, False)
                ramp.record(n, report["verdict"])
                reports.append(report)
        finally:
            self.stop_workers()
        reports.sort(key=lambda r: r["concurrent"])
        return {
            "schema": SCHEMA,
            "label": a.label,
            "note": a.note,
            "url": a.url,
            "environment": {
                "os": "linux",
                "arch": platform.machine(),
                "logical_cores": len(os.sched_getaffinity(0)),
                "release_build": True,
                "not_linux_run": False,
                "banner": None,
            },
            "settings": {
                "fixture": a.fixture,
                "turns_per_call": self.turns_per_call,
                "steps": steps,
                "rounds": a.rounds,
                "warmup_calls": a.warmup_calls,
                "spread_ms": a.spread_ms,
                "min_gap_ms": a.min_gap_ms,
                "pause_ms": a.pause_ms,
                "cooldown_ms": a.cooldown_ms,
                "floor_ms": a.floor_ms,
                "slo_over_floor_ms": a.slo_over_floor_ms,
                "slo_limit_ms": a.floor_ms + a.slo_over_floor_ms,
                "max_bad_rate": a.max_bad_rate,
                "lateness_limit_ms": a.lateness_limit_ms,
                "late_share_limit": a.late_share_limit,
                "resolution": a.resolution,
                "keep_going": a.keep_going,
                "target_pid": a.target_pid,
                "target_tree": True,
            },
            "engine": None,
            "warmup": warmup,
            "steps": reports,
            "conclusion": conclude(reports),
        }

    def settle(self) -> None:
        """Waits (up to --settle-ms) until the agent container is idle again: a process-per-job
        worker ends one process per call, and a step must not start on the previous step's
        teardown. Reported in the note of the run; the other systems have no such tail."""
        a = self.args
        if a.settle_ms <= 0:
            return
        deadline = time.monotonic() + a.settle_ms / 1000
        while time.monotonic() < deadline:
            self.samples.refresh()
            rows = [r for r in self.samples.by_name.get("agent", []) if r["t"] >= time.monotonic() - 2.0 and r["cpu_usec"] is not None]
            if len(rows) >= 2 and rows[-1]["t"] > rows[0]["t"]:
                cores = (rows[-1]["cpu_usec"] - rows[0]["cpu_usec"]) / 1e6 / (rows[-1]["t"] - rows[0]["t"])
                if cores < a.settle_cores:
                    return
            time.sleep(0.25)

    @staticmethod
    def progress(report: dict, warmup: bool) -> None:
        d = report["v2v"]
        print(
            f"{'warm-up' if warmup else 'step'} {report['concurrent']:>4}: {report['verdict']:<9} "
            f"calls {report['calls']} (failed {report['calls_failed']}) answered {report['turns_answered']}/{report['turns_expected']} "
            f"p50 {d['p50_us'] // 1000 if d else '-'} p95 {d['p95_us'] // 1000 if d else '-'} ms "
            f"agent cpu {(report['target'] or {}).get('cpu_cores')} late {report['max_send_lateness_us'] // 1000} ms "
            + "; ".join(report["reasons"]),
            flush=True,
        )


# ---------------------------------------------------------------------------------------------
# Markdown (the layout of report.rs)


def ms_us(us: int) -> str:
    return f"{us / 1000:.0f}"


def mb(b: int) -> str:
    return f"{b / (1024 * 1024):.0f}"


def step_row(s: dict) -> str:
    def d(key: str) -> str:
        return ms_us(s["v2v"][key]) if s["v2v"] else "-"

    def cores(u: dict | None) -> str:
        return f"{u['cpu_cores']:.2f}" if u and u.get("cpu_cores") is not None else "-"

    rss = "-"
    if s["target"]:
        rss = f"{mb(s['rss_before_bytes']) if s['rss_before_bytes'] is not None else '-'} / {mb(s['target']['rss_peak_bytes'])}"
    over = f"{s['p95_over_floor_us'] / 1000:+.0f}" if s["p95_over_floor_us"] is not None else "-"
    verdict = {"pass": "pass", "fail": "FAIL", "generator_limited": "generator limited"}[s["verdict"]]
    return (
        f"| {s['concurrent']} | {s['calls']} ({s['calls_failed']}) | {s['turns_answered']} / {s['turns_expected']} | "
        f"{s['talked_over']} / {s['no_response']} / {s['bot_speaking']} | {d('p50_us')} | {d('p95_us')} | {d('p99_us')} | {d('max_us')} | "
        f"{over} | {cores(s['target'])} | {rss} | {cores(s['client'])} | {ms_us(s['max_send_lateness_us'])} | {verdict} |\n"
    )


LIMITS = {
    "found": "the next step up did not pass",
    "not_reached": "every step passed: a lower bound, the limit is above the ramp",
    "generator_limited": "the load generator ran out first: a lower bound, not the server's limit",
    "first_step_failed": "even the smallest step did not pass",
}


def markdown(report: dict, environment: list[str]) -> str:
    s = report["settings"]
    out = [f"# Capacity: {report['label']}\n\n"]
    for line in environment:
        out.append(f"- {line}\n")
    out.append(f"- target: `{report['url']}`\n")
    if report.get("note"):
        out.append(f"- server: {report['note']}\n")
    warm = report.get("warmup")
    warm_note = f" (the warm-up failed: {'; '.join(warm['reasons'])})" if warm and warm["verdict"] == "fail" else ""
    out.append(
        f"- every call replays `{s['fixture']}` ({s['turns_per_call']} utterances) in real time; {s['rounds']} call(s) per slot, "
        f"slots start spread over {s['spread_ms']} ms; steps {s['steps']}; {s['warmup_calls']} warm-up call(s) first, not counted{warm_note}\n"
    )
    out.append(
        f"- SLO: p95 V2V at most floor {s['floor_ms']} ms + {s['slo_over_floor_ms']} ms = **{s['slo_limit_ms']} ms**, no failed call, "
        f"at most {s['max_bad_rate'] * 100:.1f} % of turns lost or not clean; a healthy step in which more than "
        f"{s['late_share_limit'] * 100:.1f} % of the frames went out over {s['lateness_limit_ms']} ms late is generator limited\n"
    )
    out.append(
        f"- CPU / RSS: read from the cgroup of the agent container (pid {s['target_pid']} is its init) every sample period: "
        "CPU is the container's `usage_usec` (exited job processes included); the RSS column is **PSS** summed over the container's "
        "processes (forked job processes share pages, which a plain sum of `VmRSS` would count once per process: see the second table)\n"
    )
    c = report["conclusion"]
    first = f" (first failure at {c['first_failure']})" if c["first_failure"] is not None else ""
    result = f"max {c['max_concurrent']} concurrent calls within the SLO" if c["max_concurrent"] is not None else "no passing step"
    out.append(f"\n**Result: {result}**; {LIMITS[c['limit']]}{first}.\n\n")
    out.append(
        "| concurrent | calls (failed) | answered / expected | talked over / no response / bot speaking | V2V p50 | p95 | p99 | max (ms) "
        "| p95 over floor (ms) | target CPU (cores) | target RSS before / peak (MB) | harness CPU (cores) | send lateness (ms) | result |\n"
        "|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|\n"
    )
    for st in report["steps"]:
        out.append(step_row(st))
    out.append(
        "\nThe LiveKit server (SFU) and memory by other measures, per step (not counted in the target columns):\n\n"
        "| concurrent | SFU CPU (cores) | SFU RSS peak (MB) | target plain VmRSS-sum peak (MB) | target cgroup memory peak (MB) | target processes (peak) |\n"
        "|---:|---:|---:|---:|---:|---:|\n"
    )
    for st in report["steps"]:
        sfu, tgt = st.get("sfu"), st.get("target")
        sfu_cpu = f"{sfu['cpu_cores']:.2f}" if sfu and sfu.get("cpu_cores") is not None else "-"
        sfu_rss = mb(sfu["rss_peak_bytes"]) if sfu else "-"
        pss = mb(tgt["rss_sum_peak_bytes"]) if tgt else "-"
        cg = mb(tgt["cgroup_memory_peak_bytes"]) if tgt else "-"
        procs = tgt["processes"] if tgt else "-"
        out.append(f"| {st['concurrent']} | {sfu_cpu} | {sfu_rss} | {pss} | {cg} | {procs} |\n")
    notes = []
    for st in report["steps"]:
        if st["verdict"] != "pass":
            notes.append(f"- {st['concurrent']} concurrent: {'; '.join(st['reasons'])}")
        for f in st["failures"]:
            notes.append(f"- {st['concurrent']} concurrent, failed call: {f}")
    if notes:
        out.append("\nWhy steps did not pass:\n\n" + "\n".join(notes) + "\n")
    out.append(
        "\nPercentiles are nearest-rank over the answered turns of the step; with fewer than 20 samples p95 is the largest or "
        "second largest sample, and a mock-provider run measures the framework, not the providers (see `docs/LATENCY.md`).\n"
    )
    return "".join(out)


# ---------------------------------------------------------------------------------------------


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--url", default=os.environ.get("LIVEKIT_URL", "ws://127.0.0.1:7880"))
    p.add_argument("--api-key", default=os.environ.get("LIVEKIT_API_KEY", "devkey"))
    p.add_argument("--api-secret", default=os.environ.get("LIVEKIT_API_SECRET", ""))
    p.add_argument("--fixtures-dir", required=True)
    p.add_argument("--fixture", default="en_two_utterances")
    p.add_argument("--steps", default="1,5,10,20", type=lambda s: [int(x) for x in s.split(",")])
    p.add_argument("--resolution", type=int, default=0)
    p.add_argument("--keep-going", action="store_true")
    p.add_argument("--rounds", type=int, default=2)
    p.add_argument("--warmup-calls", type=int, default=1)
    p.add_argument("--spread-ms", type=int, default=4000)
    p.add_argument("--min-gap-ms", type=int, default=4000)
    p.add_argument("--pause-ms", type=int, default=200)
    p.add_argument("--cooldown-ms", type=int, default=3000)
    p.add_argument("--settle-ms", type=int, default=0, help="after the cooldown, wait until the agent container is idle (0: off)")
    p.add_argument("--settle-cores", type=float, default=0.15)
    p.add_argument("--floor-ms", type=int, default=950)
    p.add_argument("--slo-over-floor-ms", type=int, default=250)
    p.add_argument("--max-bad-rate", type=float, default=0.0)
    p.add_argument("--lateness-limit-ms", type=int, default=100)
    p.add_argument("--late-share-limit", type=float, default=0.01)
    p.add_argument("--response-timeout-ms", type=int, default=10000)
    p.add_argument("--connect-timeout-ms", type=int, default=5000)
    p.add_argument("--ready-timeout-ms", type=int, default=10000)
    p.add_argument("--workers", type=int, default=8)
    p.add_argument("--sample-period", type=float, default=0.5)
    p.add_argument("--target", action="append", default=[], help="name=cgroup-dir for the sampler (agent, sfu); the client is added")
    p.add_argument("--target-pid", type=int, default=None)
    p.add_argument("--label", default="livekit (4 CPUs, 4g, Linux)")
    p.add_argument("--note", default=None)
    p.add_argument("--command", default=None, help="the command line to print in the markdown")
    p.add_argument("--dump-turns", default=None, help="append every call's turns (JSON lines) to this file")
    p.add_argument("--out-json", required=True)
    p.add_argument("--out-md", required=True)
    return p.parse_args()


def own_cgroup() -> str | None:
    try:
        for line in Path("/proc/self/cgroup").read_text().splitlines():
            if line.startswith("0::"):
                path = "/sys/fs/cgroup" + line[3:]
                return path if os.path.isdir(path) else None
    except OSError:
        pass
    return None


def main() -> None:
    args = parse_args()
    sample_file = Path(f"/tmp/loadgen-samples-{os.getpid()}.jsonl")
    targets = list(args.target)
    me = own_cgroup()
    if me:
        targets.append(f"client={me}")
    sampler = None
    if targets:
        sampler = subprocess.Popen(
            [sys.executable, str(Path(__file__).with_name("sampler.py")), "--out", str(sample_file), "--period", str(args.sample_period), *targets]
        )
        time.sleep(1.0)
    samples = Samples(sample_file)
    try:
        report = Coordinator(args, samples).run()
    finally:
        if sampler:
            sampler.terminate()
            sampler.wait(timeout=10)
    Path(args.out_json).write_text(json.dumps(report, indent=2) + "\n")
    env = [
        f"command: `{args.command or ' '.join(sys.argv)}`",
        f"loadgen.py (Python {platform.python_version()}, livekit rtc SDK), linux {platform.machine()}, "
        f"{report['environment']['logical_cores']} logical cores available to the generator, {args.workers} worker processes "
        "(one asyncio loop each; processes, not a CPU limit: the container's cpuset is the limit)",
        f"live: a LiveKit server at the URL; the load generator is this container ({args.workers} worker processes)",
    ]
    Path(args.out_md).write_text(markdown(report, env))
    print(f"result: {report['conclusion']}", flush=True)


if __name__ == "__main__":
    main()
