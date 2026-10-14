#!/usr/bin/env python3
"""Turns the raw output of a live V2V comparison run into the committed result files.

Inputs, per block (one start of a system plus one `brisvox-bench v2v --url` run):
  <dir>/<label>.json            the bench's JSON report (`--out-json`)
  <dir>/<label>.md              the bench's markdown report (`--out-md`)
  <dir>/<label>.<system>.log    the system's log (brisvox-server `turn finished` lines at INFO,
                                or the Pipecat baseline's `PCMETRIC` lines, `--provider-metrics`)

Outputs `<out-prefix>.md` and `<out-prefix>.json`: V2V percentiles for the first block (the run
as specified) and for all blocks pooled, the provider latencies each system's own timers saw, the
residual that is left for the framework, every turn, and the failures. No key is read or written;
log lines are scanned for the few fields below and nothing else is copied.

Calls that overlap (`--concurrency N`): the bench ran N calls at once, so the system's log
lines of different calls interleave. Provider timers are therefore matched to the bench's turns by
call, not by position: Pipecat's `PCMETRIC ... call=<id>` lines carry the call id, brisvox's
`turn finished` lines sit in a `session{id=sK}` span and its `call admitted session=sK call_id=ws-N`
line says which call the session is (the bench report holds the same `ws-N`). Logs without those
markers (the 2026-10-13 run) fall back to the old order matching, which is only sound for one call at
a time.

Usage:
  scripts/bench_live_report.py --system brisvox|pipecat --dir DIR --blocks a,b,c \
      --out-prefix bench/results/v2v-live-brisvox-windows-2026-10-13 [--date 2026-10-13] [--lang en] \
      [--tts cartesia|deepgram_aura] [--concurrency N] [--pin openai]
"""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import re
import statistics
import sys
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-9;]*m")
KV = re.compile(r"(\w+)=(-?\d+)")
PCMETRIC = re.compile(r"PCMETRIC ttfb (\w+?)Service#(\d+) (\S+) ([0-9.]+)(?: call=(\S+))?")
PCUPSTREAM = re.compile(r"PCUPSTREAM \S+ provider=(\S+) model=(\S+)")
SESSION = re.compile(r"session\{id=(\w+)\}")
ADMITTED = re.compile(r"call admitted .*?session=(\w+)(?: call_id=(\S+))?")
UPSTREAM = re.compile(r'llm upstream .*?upstream_provider="([^"]*)" upstream_model="([^"]*)"')
VAD_STOP_MS = 100.0  # Pipecat's mapped VAD stop (agent_mapping.BRISVOX_VAD_MIN_SILENCE_MS)


def percentile(values: list[float], p: float) -> float | None:
    """Nearest rank, like the bench (`brisvox_bench::v2v::report`)."""
    if not values:
        return None
    ordered = sorted(values)
    rank = max(1, math.ceil(p / 100 * len(ordered)))
    return ordered[min(rank, len(ordered)) - 1]


def stats(values: list[float]) -> dict:
    if not values:
        return {"n": 0}
    return {
        "n": len(values),
        "min": min(values),
        "p50": percentile(values, 50),
        "p90": percentile(values, 90),
        "p95": percentile(values, 95),
        "p99": percentile(values, 99),
        "max": max(values),
        "mean": statistics.fmean(values),
    }


def read_lines(path: Path):
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            yield ANSI.sub("", line.rstrip("\n"))


def brisvox_turns(log: Path) -> list[dict]:
    """The server's own per-turn timeline (`brisvox::timeline`, `turn finished`), in order. Each
    turn has `call_id` (the `ws-N` of the call, from the session span and the `call admitted` line)
    when the log has those markers, else None."""
    session_call: dict[str, str] = {}
    lines = list(read_lines(log))
    for line in lines:
        m = ADMITTED.search(line)
        if m and m.group(2) and m.group(2) != "-":
            session_call[m.group(1)] = m.group(2)
    turns = []
    for line in lines:
        if "turn finished" not in line:
            continue
        fields = {k: int(v) for k, v in KV.findall(line.split("turn finished", 1)[1])}
        sm = SESSION.search(line)
        turns.append(
            {
                "call_id": session_call.get(sm.group(1)) if sm else None,
                "interrupted": "interrupted=true" in line,
                "stt_final_ms": fields.get("stt_final_delay_us", 0) / 1000 if "stt_final_delay_us" in fields else None,
                "llm_ttft_ms": fields.get("llm_ttft_us", 0) / 1000 if "llm_ttft_us" in fields else None,
                "llm_first_sentence_ms": fields["llm_first_sentence_us"] / 1000 if "llm_first_sentence_us" in fields else None,
                "tts_ttfb_ms": fields["tts_ttfb_us"] / 1000 if "tts_ttfb_us" in fields else None,
                "end_of_turn_ms": fields["end_of_turn_delay_us"] / 1000 if "end_of_turn_delay_us" in fields else None,
                "response_latency_ms": fields["response_latency_us"] / 1000 if "response_latency_us" in fields else None,
                "llm_rounds": fields.get("llm_rounds"),
            }
        )
    return turns


def pipecat_turns(log: Path) -> list[dict]:
    """Pipecat's service TTFB timers, grouped per call (service instance number) and per turn."""
    events: dict[str, list[tuple[str, float]]] = {}
    call_of: dict[str, str | None] = {}
    order: list[str] = []
    for line in read_lines(log):
        m = PCMETRIC.search(line)
        if not m:
            continue
        kind, instance, model, ms = m.group(1), int(m.group(2)), m.group(3), float(m.group(4))
        if model == "-" and ms == 0.0:  # the zero-value metrics every service pushes at start
            continue  # (a service without a model name, Deepgram TTS, still has real values)
        # a line names its call; an old log only has the service instance number (per call)
        has_id = bool(m.group(5)) and m.group(5) != "-"
        call = m.group(5) if has_id else f"instance-{instance}"
        if call not in events:
            events[call] = []
            order.append(call)
            call_of[call] = call if has_id else None
        events[call].append((kind, ms))
    turns: list[dict] = []
    for call in order:
        group: dict | None = None
        for kind, ms in events[call]:
            if kind == "DeepgramSTT":
                if group is not None:
                    turns.append(group)
                group = {"call_id": call_of[call], "stt_ttfb_ms": ms, "llm_ttfb_ms": None,
                         "tts_ttfb_ms": None, "llm_requests": 0}
            elif group is not None and kind == "OpenAILLM":
                group["llm_ttfb_ms"] = ms  # the last request of a turn is the one that was answered
                group["llm_requests"] += 1
            elif group is not None and kind in ("CartesiaTTS", "DeepgramTTS"):
                group["tts_ttfb_ms"] = ms
        if group is not None:
            turns.append(group)
    return turns


def upstream_counts(system: str, log: Path) -> dict:
    """Which upstream provider and model each LLM response reported (brisvox `llm upstream`,
    Pipecat `PCUPSTREAM`), and how many LLM requests the system made (brisvox: `llm request
    finished`; Pipecat: LLM TTFB timers, which also fire for a request cancelled before a chunk)."""
    providers: dict[str, int] = {}
    models: dict[str, int] = {}
    requests = 0
    for line in read_lines(log):
        if system == "brisvox":
            m = UPSTREAM.search(line)
            if m:
                providers[m.group(1)] = providers.get(m.group(1), 0) + 1
                models[m.group(2)] = models.get(m.group(2), 0) + 1
            if "llm request finished" in line:
                requests += 1
        else:
            m = PCUPSTREAM.search(line)
            if m:
                providers[m.group(1)] = providers.get(m.group(1), 0) + 1
                models[m.group(2)] = models.get(m.group(2), 0) + 1
            m = PCMETRIC.search(line)
            if m and m.group(1) == "OpenAILLM" and m.group(3) != "-":
                requests += 1
    return {"providers": providers, "models": models, "requests": requests}


def match_provider_turns(bench_runs: list[dict], prov: list[dict]) -> list[dict | None]:
    """The provider timeline of each bench turn (a list parallel to the bench turns of `bench_runs`,
    in run order). By call id when the provider turns carry one (overlapping calls); else by
    position (one call at a time, logs of the 2026-10-13 run)."""
    n = sum(len(r["turns"]) for r in bench_runs)
    if prov and all(t.get("call_id") for t in prov):
        by_call: dict[str, list[dict]] = {}
        for t in prov:
            by_call.setdefault(t["call_id"], []).append(t)
        out: list[dict | None] = []
        for r in bench_runs:
            mine = by_call.get(r.get("call_id"), [])
            out.extend(mine[i] if i < len(mine) else None for i in range(len(r["turns"])))
        return out
    return [prov[i] if i < len(prov) else None for i in range(n)]


def bench_turns(report: dict) -> list[dict]:
    out = []
    for run in report.get("runs", []):
        if run.get("failure"):
            continue
        for turn in run.get("turns", []):
            out.append(
                {
                    "fixture": run.get("fixture"),
                    "run": run.get("run"),
                    "turn": turn.get("index"),
                    "lang": turn.get("lang"),
                    "outcome": turn.get("outcome"),
                    "v2v_ms": turn["v2v_us"] / 1000 if turn.get("v2v_us") is not None else None,
                }
            )
    return out


def fmt(x, digits=0):
    if x is None:
        return "-"
    return f"{x:.{digits}f}"


def row(name: str, s: dict) -> str:
    if not s.get("n"):
        return f"| {name} | 0 | - | - | - | - | - | - | - |"
    p99 = s.get("p99")
    return (
        f"| {name} | {s['n']} | {fmt(s['min'])} | {fmt(s['p50'])} | {fmt(s['p90'])} | "
        f"{fmt(s['p95'])} | {fmt(p99)} | {fmt(s['max'])} | {fmt(s['mean'])} |"
    )


TABLE_HEAD = (
    "| metric (ms) | n | min | p50 | p90 | p95 | p99 | max | mean |\n"
    "|---|---:|---:|---:|---:|---:|---:|---:|---:|"
)

IPV6_NOTE_AURA = (
    "- **IPv6 note.** This machine's IPv6 route silently drops packets. brisvox races address\n"
    "  families for its provider websockets; the Pipecat baseline enables asyncio's happy-eyeballs\n"
    "  racing for its websocket services (`bot.py`), so connection setup is not part of the difference.\n"
    "  (The Cartesia handshake failure that motivated this does not apply here: the TTS is Deepgram.)\n"
)


def banner_text(doc: dict, banner: str) -> str:
    """The banner with the Cartesia-specific IPv6 paragraph replaced when the TTS is not Cartesia."""
    if doc.get("tts") != "deepgram_aura":
        return banner
    start = banner.index("- **IPv6 note.**")
    end = banner.index("{sample_text}")
    return banner[:start] + IPV6_NOTE_AURA + banner[end:]


TTS_NAMES = {
    "cartesia": "Cartesia `sonic-3.6` (websocket, 24 kHz, the voice of `CARTESIA_VOICE_ID`)",
    "deepgram_aura": "Deepgram Aura `aura-2-thalia-en` (websocket `/v1/speak`, linear16, 24 kHz)",
}

BANNER = """\
## Environment

- **Windows, real providers over the public internet.** Machine: {platform}, {cores} logical cores,
  Python {python}. Both systems and the benchmark client ran on this one machine, one system at a
  time, sequentially (never concurrently), against loopback. The machine was shared with other
  work (builds, a Docker/WSL benchmark in another session), so absolute numbers carry noise; the
  comparison interleaves the two systems block by block to spread that noise.
- **Not a Linux run.** The Windows timer ticks every ~15.6 ms; the largest client send lateness of
  each block is in its raw report below (a late frame delays the end of speech the server sees by as
  much). `docs/LATENCY.md` asks for Linux numbers for target claims.
- **Providers (identical for both systems, one agent file: `bench/pipecat_baseline/{agent_file}.toml`):**
  STT Deepgram `nova-3` (live websocket, endpointing 300 ms, `smart_format`); LLM
  `openai/gpt-6-luna` through **OpenRouter** (`https://openrouter.ai/api/v1`, Chat Completions,
  streaming, `max_tokens` 120, `reasoning_effort` `none`: the model thinks by default, which adds
  about a second to the first token){pin_text}; TTS {tts_text}. Turns: adaptive, Smart Turn v3.2
  (the same ONNX file on both sides), Silero VAD v6.2.3, one-sentence prompt.
- **IPv6 note.** This machine's IPv6 route silently drops packets while `api.cartesia.ai`
  and `openrouter.ai` publish AAAA records. brisvox races address families (RFC 8305) for its
  provider websockets and relies on reqwest for HTTP. Pipecat 1.12.0 as shipped connects its
  provider websockets one address at a time: the first Cartesia handshake of a call timed out
  after 10 s ("timed out during opening handshake") and that call's first turn got no audio. The
  baseline bot therefore enables asyncio's happy-eyeballs racing for Pipecat's websocket services
  (`bot.py` `enable_happy_eyeballs`, `--no-happy-eyeballs` restores Pipecat as shipped). Without
  that fix Pipecat fails on this host; with it the connection setup is no longer part of the
  difference.
{sample_text}
- Keys never appear in any of these files; they were passed through the environment only.
"""


def build_markdown(doc: dict) -> str:
    system = doc["system"]
    env = doc["environment"]
    conc = doc.get("concurrency", 1)
    title = f"# Live V2V, {system}, Windows, real providers ({doc['date']})"
    if conc > 1:
        title = f"# Live V2V, {system}, {conc} concurrent calls, Windows, real providers ({doc['date']})"
    lines = [title, ""]
    pooled = doc["v2v"]["pooled"]["all"]["n"]
    if conc > 1:
        sample_text = (
            f"- **Sample size and concurrency.** {conc} calls at once, 2 turns per call; {len(doc['blocks'])} blocks "
            f"({pooled} turns) interleaved with the other system's blocks (b1 p1 b2 p2 ...), so both saw the "
            "same provider conditions. Every call plays the same fixture, so the {conc} calls speak at "
            "nearly the same moment and the providers see {conc} simultaneous requests. p99 over {pooled} "
            "turns is the 99th value, one above the 2nd largest."
        ).replace("{conc}", str(conc)).replace("{pooled}", str(pooled))
    else:
        sample_text = (
            "- **Sample size.** 2 turns per call. The first block ({spec_runs} calls, {spec_turns} turns) is the run as\n"
            "  specified; the later blocks were added because the LLM provider's variance (OpenRouter first\n"
            "  token 0.6 s to 3 s for the same request) is larger than the framework differences being\n"
            "  measured. Percentiles of {pooled} turns are still a small sample: p90 and p95 are the 2nd and 3rd\n"
            "  largest values, not tail estimates."
        ).format(spec_runs=doc["blocks"][0]["runs"], spec_turns=doc["blocks"][0]["runs"] * 2, pooled=pooled)
    routing = doc.get("routing") or {}
    pin_text = ""
    if routing.get("pinned"):
        pin_text = (
            f", upstream pinned with `provider.order` `{routing['pinned']}` and `allow_fallbacks` false"
        )
    lines.append(
        banner_text(doc, BANNER).format(
            platform=env["platform"],
            cores=env["logical_cores"],
            python=env["python"],
            lang=doc["lang"],
            agent_file=doc.get("agent_file", "agent_live_" + doc["lang"]),
            pin_text=pin_text,
            tts_text=TTS_NAMES.get(doc.get("tts", "cartesia"), doc.get("tts", "")),
            sample_text=sample_text,
        )
    )
    lines.append(f"- System under test: {doc['system_description']}")
    lines.append(f"- Blocks (in order): {', '.join(b['label'] + ' (' + str(b['runs']) + ' calls)' for b in doc['blocks'])}")
    lines.append("")
    if routing:
        lines.append("## LLM route and upstream provider")
        lines.append("")
        lines.append(
            f"Model `{routing['model']}`, `reasoning_effort` `{routing['reasoning_effort']}`, pinned upstream "
            f"`{routing['pinned']}` (`allow_fallbacks` false). Upstream reported by the first stream chunk of "
            f"each response: {json.dumps(routing['reported_providers'])} "
            f"(models: {json.dumps(routing['reported_models'])}); LLM requests logged: {routing['requests']}, "
            f"responses that reported an upstream: {routing['responses_with_upstream']}. "
            f"**All reported upstreams are the pinned one: {routing['all_pinned']}.**"
        )
        lines.append("")
    lines.append("## Voice-to-voice (V2V)")
    lines.append("")
    lines.append(
        "V2V = arrival at the client of the first non-silent bot audio after the ground-truth end "
        "of the user's speech, minus that end of speech (`docs/LATENCY.md`). Nearest-rank "
        "percentiles over answered turns."
    )
    lines.append("")
    lines.append(TABLE_HEAD)
    first_name = "V2V, first block" if conc > 1 else "V2V, first block (as specified)"
    lines.append(row(first_name, doc["v2v"]["spec"]["all"]))
    lines.append(row("V2V, all blocks pooled", doc["v2v"]["pooled"]["all"]))
    for pos in ("0", "1"):
        s = doc["v2v"]["pooled"]["by_turn"].get(pos)
        if s:
            lines.append(row(f"V2V, pooled, turn {pos}", s))
    lines.append("")
    if doc.get("v2v_by_block"):
        lines.append("V2V p50 per block (ms): " + ", ".join(f"{k} {fmt(v['p50'])}" for k, v in doc["v2v_by_block"].items()) + ".")
        lines.append("")
    lines.append(f"Outcomes (pooled): {json.dumps(doc['outcomes'])}.")
    lines.append("")
    lines.append("## Provider latencies as this system's own timers saw them")
    lines.append("")
    lines.append(doc["provider_note"])
    lines.append("")
    lines.append(TABLE_HEAD)
    for name, s in doc["provider_latency"].items():
        lines.append(row(name, s))
    lines.append("")
    lines.append("## What is left for the framework")
    lines.append("")
    lines.append(doc["residual_note"])
    lines.append("")
    lines.append(TABLE_HEAD)
    for name, s in doc["residual"].items():
        lines.append(row(name, s))
    lines.append("")
    lines.append("## Failures")
    lines.append("")
    for k, v in doc["failures"].items():
        lines.append(f"- {k}: {v}")
    lines.append("")
    lines.append("## Every turn")
    lines.append("")
    cols = doc["turn_columns"]
    lines.append("| " + " | ".join(cols) + " |")
    lines.append("|" + "|".join("---:" if c not in ("block", "outcome") else "---" for c in cols) + "|")
    for t in doc["turns"]:
        cells = []
        for c in cols:
            v = t.get(c)
            cells.append("-" if v is None else fmt(v, 0) if isinstance(v, (int, float)) else str(v))
        lines.append("| " + " | ".join(cells) + " |")
    lines.append("")
    lines.append("## Raw bench reports (headers)")
    lines.append("")
    for b in doc["blocks"]:
        lines.append(f"### {b['label']}")
        lines.append("")
        lines.extend(b["bench_header"])
        lines.append("")
    return "\n".join(lines) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--system", choices=["brisvox", "pipecat"], required=True)
    ap.add_argument("--dir", required=True, type=Path)
    ap.add_argument("--blocks", required=True, help="comma separated labels; the first is the run as specified")
    ap.add_argument("--out-prefix", required=True, type=Path)
    ap.add_argument("--date", default="2026-10-13")
    ap.add_argument("--lang", default="en")
    ap.add_argument("--tts", choices=["cartesia", "deepgram_aura"], default="cartesia")
    ap.add_argument("--concurrency", type=int, default=1, help="calls at once in each block")
    ap.add_argument("--pin", default="", help="OpenRouter upstream slug both systems were pinned to")
    ap.add_argument("--model", default="openai/gpt-6-luna")
    ap.add_argument("--reasoning-effort", default="none")
    args = ap.parse_args()
    system = args.system
    labels = args.blocks.split(",")
    agent_file = f"agent_live_{args.lang}" + ("_aura" if args.tts == "deepgram_aura" else "")

    turns: list[dict] = []
    blocks: list[dict] = []
    failures = {"failed calls": 0, "no_response turns": 0, "talked_over turns": 0, "bot_speaking turns": 0,
                "turns without a provider timeline": 0, "ERROR log lines": 0, "WARN log lines": 0}
    outcomes: dict[str, int] = {}
    bench_ver = ""
    upstream = {"providers": {}, "models": {}, "requests": 0}
    for index, label in enumerate(labels):
        report = json.loads((args.dir / f"{label}.json").read_text(encoding="utf-8"))
        md_head = []
        for line in (args.dir / f"{label}.md").read_text(encoding="utf-8").splitlines():
            if line.startswith("## Summary"):
                break
            md_head.append(re.sub(r"--out-(md|json) \S+", "", line))
        bt = bench_turns(report)
        log = args.dir / f"{label}.{system}.log"
        prov = brisvox_turns(log) if system == "brisvox" else pipecat_turns(log)
        matched = match_provider_turns([r for r in report.get("runs", []) if not r.get("failure")], prov)
        got = upstream_counts(system, log)
        for key in ("providers", "models"):
            for name, n in got[key].items():
                upstream[key][name] = upstream[key].get(name, 0) + n
        upstream["requests"] += got["requests"]
        failures["failed calls"] += report.get("failed_runs", 0)
        for line in read_lines(log):
            if re.search(r"\bERROR\b", line):
                failures["ERROR log lines"] += 1
            elif re.search(r"\bWARN(ING)?\b", line):
                if "mapping:" not in line and "differs from the recommended" not in line:
                    failures["WARN log lines"] += 1
            if re.search(r"concurrency limit|concurrency_limited|status=429|Error code: 429", line):
                failures["provider concurrency / 429 refusals"] = failures.get("provider concurrency / 429 refusals", 0) + 1
        for i, t in enumerate(bt):
            outcomes[t["outcome"]] = outcomes.get(t["outcome"], 0) + 1
            rec = {"block": label, **t}
            p = matched[i] if i < len(matched) else None
            if p is None:
                failures["turns without a provider timeline"] += 1
            else:
                rec.update(p)
            rec["spec"] = index == 0
            turns.append(rec)
        blocks.append({"label": label, "runs": report["settings"]["runs"], "bench_header": md_head,
                       "provider_turns_logged": len(prov), "bench_turns": len(bt)})
        ov = report.get("overall", {})
        failures["no_response turns"] += ov.get("no_response", 0)
        failures["talked_over turns"] += ov.get("talked_over", 0)
        failures["bot_speaking turns"] += ov.get("bot_speaking", 0)
        m = re.search(r"- brisvox (\S+) \(([^)]*)\)", "\n".join(md_head))
        if m:
            bench_ver = f"brisvox-bench {m.group(1)} ({m.group(2)})"

    answered = [t for t in turns if t["outcome"] == "answered" and t["v2v_ms"] is not None]

    by_block = {}
    for label in labels:
        rows = [t["v2v_ms"] for t in answered if t["block"] == label]
        if rows:
            by_block[label] = stats(rows)

    def v2v_stats(rows):
        return {
            "all": stats([t["v2v_ms"] for t in rows]),
            "by_turn": {str(k): stats([t["v2v_ms"] for t in rows if t["turn"] == k]) for k in (0, 1)},
        }

    if system == "brisvox":
        provider_rows = {
            "STT final after end of speech (`stt_final_delay`)": [t.get("stt_final_ms") for t in answered],
            "LLM first token after the request (`llm_ttft`)": [t.get("llm_ttft_ms") for t in answered],
            "LLM first speakable chunk after the request (`llm_first_sentence`)": [t.get("llm_first_sentence_ms") for t in answered],
            "TTS first audio after the text (`tts_ttfb`)": [t.get("tts_ttfb_ms") for t in answered],
        }
        provider_note = (
            "From the server's `turn finished` timeline lines (`brisvox::timeline`, `docs/LATENCY.md`, "
            "\"Turn timeline\"), one per turn. `stt_final_delay` is counted from the VAD's backdated end "
            "of speech to the last Deepgram final; `llm_ttft` from the request written to the connection "
            "to the first text delta (OpenRouter's time to first byte included); `tts_ttfb` from the first "
            "text sent to the TTS provider to its first audio byte."
        )
        for t in answered:
            parts = [t.get("stt_final_ms"), t.get("llm_first_sentence_ms"), t.get("tts_ttfb_ms")]
            t["providers_ms"] = sum(parts) if all(x is not None for x in parts) else None
            t["residual_ms"] = t["v2v_ms"] - t["providers_ms"] if t["providers_ms"] is not None else None
            strict = [t.get("stt_final_ms"), t.get("llm_ttft_ms"), t.get("tts_ttfb_ms")]
            t["residual_strict_ms"] = t["v2v_ms"] - sum(strict) if all(x is not None for x in strict) else None
            t["server_response_ms"] = t.get("response_latency_ms")
            t["transport_and_eos_ms"] = (
                t["v2v_ms"] - t["response_latency_ms"] if t.get("response_latency_ms") is not None else None
            )
            t["overhead_in_server_ms"] = (
                t["response_latency_ms"] - t["providers_ms"]
                if t.get("response_latency_ms") is not None and t["providers_ms"] is not None
                else None
            )
        residual = {
            "V2V minus (STT final + LLM first token + TTS first byte): the definition used for Pipecat below": [t.get("residual_strict_ms") for t in answered],
            "V2V minus (STT final + LLM first chunk + TTS first byte)": [t.get("residual_ms") for t in answered],
            "  of which inside the server (server response latency minus the three providers)": [t.get("overhead_in_server_ms") for t in answered],
            "  of which client side (V2V minus the server's response latency: transport, EOS definition, frame grid)": [t.get("transport_and_eos_ms") for t in answered],
        }
        residual_note = (
            "Per turn: V2V minus the provider times above. The first row uses the LLM's first token, the "
            "same definition as the Pipecat file (there the wait for the first sentence is not timed, so it "
            "stays in the residual); the second row uses the first speakable chunk (the wait for a clause is "
            "provider streaming time). What remains is everything the engine and the transport add: turn "
            "detection after the final, hand-offs, the output frame grid, the websocket, and the difference "
            "between the VAD's end of speech and the fixture's ground truth (~150 ms, in both systems). "
            "The server's own response latency (end of speech to the first audio frame handed to the "
            "transport) splits the second row into the part inside the server and the client-side part."
        )
        system_description = (
            "`brisvox-server` (release, features `provider-deepgram`, `provider-openai-compatible`, "
            "`provider-cartesia`, `provider-deepgram-aura`, `smart-turn-ort`, `silero-tract`), adaptive turns with Smart Turn v3.2, "
            "agent file `bench/pipecat_baseline/{}.toml`.".format(agent_file)
        )
    else:
        provider_rows = {
            "STT final after the VAD stop (Pipecat STT TTFB)": [t.get("stt_ttfb_ms") for t in answered],
            "STT final after end of speech (TTFB + 100 ms VAD stop)": [
                t["stt_ttfb_ms"] + VAD_STOP_MS if t.get("stt_ttfb_ms") is not None else None for t in answered
            ],
            "LLM first token after the request (Pipecat LLM TTFB)": [t.get("llm_ttfb_ms") for t in answered],
            "TTS first audio after the text (Pipecat TTS TTFB)": [t.get("tts_ttfb_ms") for t in answered],
        }
        provider_note = (
            "From Pipecat's own service timers (`enable_metrics`, logged by the bot as `PCMETRIC`, "
            "`--provider-metrics`). STT TTFB is counted from the VAD stop (100 ms of silence after the end of "
            "speech, 32 ms windows) to the final transcript; LLM TTFB from the request to the first token; TTS "
            "TTFB from the first text to the first audio. A turn the user resumed mid-sentence can show more "
            "than one LLM request; the last one is the answered one (`llm_requests` in the JSON)."
        )
        for t in answered:
            parts = [t.get("stt_ttfb_ms"), t.get("llm_ttfb_ms"), t.get("tts_ttfb_ms")]
            t["providers_ms"] = sum(parts) + VAD_STOP_MS if all(x is not None for x in parts) else None
            t["residual_ms"] = t["v2v_ms"] - t["providers_ms"] if t["providers_ms"] is not None else None
        residual = {
            "V2V minus (100 ms VAD stop + STT TTFB + LLM TTFB + TTS TTFB)": [t.get("residual_ms") for t in answered],
        }
        residual_note = (
            "Per turn: V2V minus the three provider times above (STT from the end of speech, the LLM to its "
            "first token, TTS to its first byte). It is not the same residual as brisvox's: Pipecat does not "
            "time the wait for the first sentence (its aggregator holds text until a sentence ends) nor the "
            "Smart Turn run, so those, the soxr resampler's 100 ms bursts, the serializer and the pacer are in "
            "this number, together with the transport and the end-of-speech definition."
        )
        system_description = (
            "Pipecat 1.12.0 bot (`bench/pipecat_baseline/bot.py`, Python 3.13), the same agent file, "
            "Smart Turn v3.2 and Silero v6.2.3, LLM through Pipecat's `OpenAILLMService` (Chat Completions) "
            "with `base_url` OpenRouter, happy-eyeballs websocket dialing on."
        )

    provider_latency = {k: stats([x for x in v if x is not None]) for k, v in provider_rows.items()}
    residual_stats = {k: stats([x for x in v if x is not None]) for k, v in residual.items()}

    routing = None
    if args.pin:
        reported = upstream["providers"]
        routing = {
            "model": args.model,
            "reasoning_effort": args.reasoning_effort,
            "pinned": args.pin,
            "allow_fallbacks": False,
            "reported_providers": reported,
            "reported_models": upstream["models"],
            "requests": upstream["requests"],
            "responses_with_upstream": sum(reported.values()),
            # OpenRouter names the upstream "OpenAI" for the slug "openai"
            "all_pinned": bool(reported) and all(name.lower() == args.pin.lower() for name in reported),
        }
    if system == "pipecat":
        restarted = sum(1 for t in answered if (t.get("llm_requests") or 0) > 1)
        failures["answered turns with more than one LLM request (cause not established)"] = restarted
        failures["LLM requests cancelled before any chunk (no upstream to report)"] = max(
            0, upstream["requests"] - sum(upstream["providers"].values())
        )
    columns = ["block", "run", "turn", "outcome", "v2v_ms"]
    columns += (
        ["stt_final_ms", "llm_ttft_ms", "llm_first_sentence_ms", "tts_ttfb_ms", "response_latency_ms", "residual_strict_ms", "residual_ms"]
        if system == "brisvox"
        else ["stt_ttfb_ms", "llm_ttfb_ms", "tts_ttfb_ms", "llm_requests", "residual_ms"]
    )
    doc = {
        "schema": "brisvox-live-v2v/1",
        "task": "live-concurrent" if args.concurrency > 1 else "live",
        "system": system,
        "system_description": system_description,
        "date": args.date,
        "lang": args.lang,
        "tts": args.tts,
        "agent_file": agent_file,
        "concurrency": args.concurrency,
        "routing": routing,
        "v2v_by_block": by_block,
        "environment": {
            "concurrency": args.concurrency,
            "platform": platform.platform(),
            "logical_cores": os.cpu_count(),
            "python": sys.version.split()[0],
            "bench": bench_ver,
            "network": "public internet, loopback between client and system under test; IPv6 route drops packets",
            "providers": {
                "stt": "deepgram nova-3",
                "llm": f"openrouter {args.model} (reasoning_effort {args.reasoning_effort}"
                + (f", upstream pinned to {args.pin}, no fallbacks)" if args.pin else ")"),
                "tts": "cartesia sonic-3.6" if args.tts == "cartesia" else "deepgram aura-2-thalia-en",
            },
        },
        "blocks": blocks,
        "outcomes": outcomes,
        "v2v": {"spec": v2v_stats([t for t in answered if t["spec"]]), "pooled": v2v_stats(answered)},
        "provider_note": provider_note,
        "provider_latency": provider_latency,
        "residual_note": residual_note,
        "residual": residual_stats,
        "failures": failures,
        "turn_columns": columns,
        "turns": turns,
    }
    args.out_prefix.parent.mkdir(parents=True, exist_ok=True)
    args.out_prefix.with_suffix(".json").write_text(json.dumps(doc, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    args.out_prefix.with_suffix(".md").write_text(build_markdown(doc), encoding="utf-8")
    print(f"wrote {args.out_prefix}.md and .json ({len(turns)} turns)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
