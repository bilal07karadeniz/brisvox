#!/usr/bin/env python3
"""Listen-under-load harness: talk to the bot yourself while N other calls run.

Starts, all confined to the same CPU cores:
  * brisvox-server with your real providers and the WebRTC listener (your call, in a browser);
  * brisvox-bench engine-server: the production engine over mock providers, with the real Silero
    VAD and Smart Turn models when the bench was built with `--features models`, so the
    background calls cost what real calls cost on this machine (providers run elsewhere anyway);
and a static server for the web page. Then, per step, it holds N background calls (the bench's
capacity client, pinned to OTHER cores so it does not steal the server's CPU) and prints their
p50/p95 voice-to-voice and lost turns, while you talk. Press Enter to go to the next step.

Keys come from the environment only (load them first, e.g. `set -a; . ./.env; set +a`); this
script never reads .env and never prints a key.

Example (Windows, Git Bash, from the repository root):
  cargo build --release -p brisvox-server --features provider-deepgram,provider-openai-compatible,provider-cartesia,webrtc,smart-turn-ort
  cargo build --release -p brisvox-bench --features models
  set -a; . ./.env; set +a
  python scripts/listen_under_load.py --agent-file my-agent.toml --calls 50,200,500,1000 --cores 4
Then open http://localhost:8000, press Start and talk; the script prints each step's numbers.
"""
from __future__ import annotations

import argparse
import functools
import http.server
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EXE = ".exe" if os.name == "nt" else ""


def lan_ip() -> str:
    """The address the OS would use to reach the internet: browsers do not send ICE to loopback."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        try:
            s.connect(("192.0.2.1", 9))  # TEST-NET-1, nothing is sent
            return s.getsockname()[0]
        except OSError:
            return "127.0.0.1"


def pin(pid: int, cores: list[int]) -> None:
    """Confines process `pid` to `cores` (Linux: sched_setaffinity; Windows: ProcessorAffinity)."""
    if hasattr(os, "sched_setaffinity"):
        os.sched_setaffinity(pid, set(cores))
        return
    mask = sum(1 << c for c in cores)
    subprocess.run(
        ["powershell.exe", "-NoProfile", "-Command",
         f"(Get-Process -Id {pid}).ProcessorAffinity = {mask}"],
        check=True, capture_output=True,
    )


def port_free(host: str, port: int) -> bool:
    """True when nothing listens on host:port (a leftover process would answer our checks)."""
    try:
        with socket.create_connection((host, port), timeout=0.5):
            return False
    except OSError:
        return True


def wait_port(host: str, port: int, what: str, timeout_s: float = 60.0) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=1):
                return
        except OSError:
            time.sleep(0.5)
    sys.exit(f"{what} did not start listening on {host}:{port}")


def serve_page(directory: Path, port: int) -> http.server.ThreadingHTTPServer:
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=str(directory))
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", port), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--agent-file", required=True, help="agent TOML for YOUR call (real providers)")
    ap.add_argument("--calls", default="25,100,250,500", help="background calls per step")
    ap.add_argument("--cores", type=int, default=4, help="cores the two servers share (0..N-1)")
    ap.add_argument("--client-cores", default="", help="generator cores (default: the next 4)")
    ap.add_argument("--rounds", type=int, default=4, help="calls each background slot plays per step (~25 s each)")
    ap.add_argument("--models-dir", default=str(ROOT / "models"))
    ap.add_argument("--no-models", action="store_true", help="background engine without the real models")
    ap.add_argument("--server-exe", default=str(ROOT / "target" / "release" / f"brisvox-server{EXE}"))
    ap.add_argument("--bench-exe", default=str(ROOT / "target" / "release" / f"brisvox-bench{EXE}"))
    ap.add_argument("--ip", default="", help="media address for WebRTC (default: this machine's LAN IP)")
    ap.add_argument("--auto", action="store_true", help="do not wait for Enter between steps")
    args = ap.parse_args()

    for exe in (args.server_exe, args.bench_exe):
        if not Path(exe).is_file():
            sys.exit(f"missing {exe}: build it first (see the docstring)")
    steps = [int(x) for x in args.calls.split(",") if x.strip()]
    server_cores = list(range(args.cores))
    client_cores = (
        [int(c) for c in args.client_cores.split(",")] if args.client_cores
        else list(range(args.cores, min(args.cores + 4, os.cpu_count() or args.cores + 4)))
    )
    ip = args.ip or lan_ip()
    for port, what in ((8081, "health"), (8088, "WebRTC signaling"), (8099, "engine-server"), (8000, "page")):
        if not port_free("127.0.0.1", port):
            sys.exit(f"port {port} ({what}) is already in use: stop the process holding it first")
    out_dir = Path(tempfile.mkdtemp(prefix="brisvox-lul-"))
    procs: list[subprocess.Popen] = []

    try:
        # 1. Your call: brisvox-server with WebRTC on the LAN address.
        env = dict(os.environ)
        env.update({
            "BRISVOX_AGENT_FILE": str(Path(args.agent_file).resolve()),
            "BRISVOX_WEBRTC_ADDR": "127.0.0.1:8088",
            "BRISVOX_WEBRTC_UDP_ADDR": f"{ip}:8088",
            "BRISVOX_WEBRTC_CANDIDATE_IP": ip,
            "BRISVOX_WEBRTC_CORS_ORIGIN": "http://localhost:8000",
            "BRISVOX_HEALTH_ADDR": "127.0.0.1:8081",
            "BRISVOX_MODELS_DIR": args.models_dir,
            "BRISVOX_MAX_SESSIONS": "4",
            "RUST_LOG": env.get("RUST_LOG", "info"),
        })
        server_log = open(out_dir / "server.log", "w", encoding="utf-8")
        server = subprocess.Popen([args.server_exe], env=env, stdout=server_log, stderr=subprocess.STDOUT)
        procs.append(server)
        wait_port("127.0.0.1", 8081, "brisvox-server")
        pin(server.pid, server_cores)

        # 2. Background load: the engine over mock providers (+ the real models).
        bench_log = open(out_dir / "engine-server.log", "w", encoding="utf-8")
        engine_cmd = [args.bench_exe, "engine-server", "--bind", "127.0.0.1:8099",
                      "--fixtures-dir", str(ROOT / "fixtures"), "--max-calls", str(max(steps) + 16),
                      "--workers", str(args.cores), "--pool-threads", str(max(1, args.cores // 2))]
        if not args.no_models:
            engine_cmd += ["--models", args.models_dir]
        engine = subprocess.Popen(engine_cmd, stdout=bench_log, stderr=subprocess.STDOUT)
        procs.append(engine)
        wait_port("127.0.0.1", 8099, "engine-server")
        if engine.poll() is not None:
            sys.exit(f"engine-server exited (code {engine.returncode}): see {out_dir / 'engine-server.log'}")
        pin(engine.pid, server_cores)

        # 3. The page.
        httpd = serve_page(ROOT / "examples" / "web-assistant" / "web", 8000)

        print(f"\nYour call: open http://localhost:8000 (server http://localhost:8088), press Start, talk.")
        print(f"Servers on cores {server_cores}; generator on cores {client_cores}; logs in {out_dir}")
        print(f"Background engine: {'mock providers + REAL models' if not args.no_models else 'mock providers, no models'}\n")
        if not args.auto:
            input("Talk to it once with no load, then press Enter to start the first step... ")

        for n in steps:
            md, js = out_dir / f"step-{n}.md", out_dir / f"step-{n}.json"
            cmd = [args.bench_exe, "capacity", "--url", "ws://127.0.0.1:8099/", "--pid", str(engine.pid),
                   "--floor-ms", "950", "--label", f"listen-under-load {n}",
                   "--steps", ",".join([str(n)] * 2), "--rounds", str(args.rounds), "--keep-going",
                   "--fixtures-dir", str(ROOT / "fixtures"), "--out-md", str(md), "--out-json", str(js)]
            print(f">> step: {n} background calls (talk now; ~{args.rounds * 25 * 2 // 60 + 1} min)")
            gen = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
            pin(gen.pid, client_cores)
            gen.wait()
            try:
                report = json.loads(js.read_text(encoding="utf-8"))
                for s in report["steps"]:
                    v = s.get("v2v") or {}
                    cpu = (s.get("target") or {}).get("cpu_cores")
                    print(f"   {s['concurrent']} calls: p50 {v.get('p50_us', 0) // 1000} ms, p95 "
                          f"{v.get('p95_us', 0) // 1000} ms, answered {s['turns_answered']}/{s['turns_expected']}, "
                          f"failed {s['calls_failed']}, engine CPU {cpu if cpu is None else round(cpu, 2)} cores, "
                          f"verdict {s['verdict']}")
            except (OSError, ValueError, KeyError) as e:
                print(f"   (no report: {e}; see {md})")
            if not args.auto and n != steps[-1]:
                input("How did your call sound? Press Enter for the next step... ")
        print(f"\nDone. Reports: {out_dir}")
        if not args.auto:
            input("Press Enter to stop the servers... ")
        httpd.shutdown()
        return 0
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()


if __name__ == "__main__":
    sys.exit(main())
