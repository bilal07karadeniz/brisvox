"""CPU and memory of Docker containers by cgroup, sampled on a process of its own.

    python sampler.py --out samples.jsonl --period 0.5 agent=/sys/fs/cgroup/system.slice/docker-<id>.scope sfu=...

One JSON line per period and cgroup:

    {"t": <time.monotonic()>, "name": "agent", "cpu_usec": ..., "mem_current": ..., "rss": ..., "pss": ..., "procs": ...}

- ``cpu_usec``: the cgroup's ``cpu.stat`` ``usage_usec``, which includes every process the container
  ever ran (a process-per-job worker starts and ends a process per call; reading the live
  processes' CPU time, as ``brisvox-bench`` does for a server tree, would lose the CPU time of the
  jobs that already exited).
- ``rss``: the sum of ``VmRSS`` over the container's processes (the definition of the Rust
  benchmark's RSS column; shared pages of forked processes are counted once per process).
- ``pss``: the sum of ``Pss`` (``smaps_rollup``): shared pages divided among the processes sharing
  them, so the sum is the memory the processes hold between them.
- ``mem_current``: the cgroup's ``memory.current`` (anonymous memory plus page cache; what the
  container's memory limit is enforced on).

``time.monotonic()`` is ``CLOCK_MONOTONIC``, shared by all processes of the host, so the load
generator can place the samples on its steps. Needs ``--pid host --cgroupns host`` and
``/sys/fs/cgroup`` mounted when run in a container. Standard library only.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import sys
import time


def read_int(path: str) -> int | None:
    try:
        with open(path) as f:
            return int(f.read().strip())
    except (OSError, ValueError):
        return None


def cpu_usec(cgroup: str) -> int | None:
    try:
        with open(os.path.join(cgroup, "cpu.stat")) as f:
            for line in f:
                if line.startswith("usage_usec "):
                    return int(line.split()[1])
    except (OSError, ValueError):
        pass
    return None


def procs(cgroup: str) -> list[int]:
    try:
        with open(os.path.join(cgroup, "cgroup.procs")) as f:
            return [int(x) for x in f.read().split()]
    except (OSError, ValueError):
        return []


def vm_rss_kb(pid: int) -> int:
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except (OSError, ValueError):
        pass
    return 0


def pss_kb(pid: int) -> int:
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith("Pss:"):
                    return int(line.split()[1])
    except (OSError, ValueError):
        pass
    return 0


def sample(name: str, cgroup: str) -> dict:
    t = time.monotonic()
    pids = procs(cgroup)
    return {
        "t": t,
        "name": name,
        "cpu_usec": cpu_usec(cgroup),
        "mem_current": read_int(os.path.join(cgroup, "memory.current")),
        "rss": sum(vm_rss_kb(p) for p in pids) * 1024,
        "pss": sum(pss_kb(p) for p in pids) * 1024,
        "procs": len(pids),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    parser.add_argument("--period", type=float, default=0.5)
    parser.add_argument("targets", nargs="+", help="name=cgroup-dir")
    args = parser.parse_args()
    targets = [tuple(t.split("=", 1)) for t in args.targets]
    stop = False

    def on_signal(*_: object) -> None:
        nonlocal stop
        stop = True

    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)
    with open(args.out, "a", buffering=1) as out:
        next_at = time.monotonic()
        while not stop:
            for name, cgroup in targets:
                out.write(json.dumps(sample(name, cgroup)) + "\n")
            next_at += args.period
            delay = next_at - time.monotonic()
            if delay > 0:
                time.sleep(delay)
            else:
                next_at = time.monotonic()
    sys.exit(0)


if __name__ == "__main__":
    main()
