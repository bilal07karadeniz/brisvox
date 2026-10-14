#!/usr/bin/env python3
"""Fold the repeats of bench/capacity_linux.sh into one summary per system.

    python3 bench/capacity_summary.py bench/results 2026-10-13

Reads capacity-<system>-linux-<date>-run<N>.json and writes capacity-<system>-linux-<date>.md and
.json: for every run the capacity (largest passing step), the first failing step, and the p95 V2V,
server CPU, CPU per call and RSS at the capacity step, then the median over the runs. The medians
of capacity are the figures docs/LATENCY.md and the README quote. Standard library only.
"""
import glob
import json
import os
import re
import statistics
import sys


def at_max(run):
    """The step the run reports as its capacity, or None when even the first step failed."""
    best = run["conclusion"].get("max_concurrent")
    if not best:
        return None
    for step in run["steps"]:
        if step["concurrent"] == best and step["verdict"] == "pass":
            return step
    return None


def first_failure_reason(run):
    first = run["conclusion"].get("first_failure")
    for step in run["steps"]:
        if step["concurrent"] == first and step["verdict"] != "pass":
            return "; ".join(step.get("reasons", []))
    return ""


def memory_bytes(target):
    """(RSS summed over the processes, PSS summed over the processes or None) of a target's usage.

    brisvox-bench writes both: `rss_peak_bytes` is the RSS sum, `pss_peak_bytes` the PSS
    sum. The LiveKit harness (bench/livekit_baseline/loadgen.py) predates that and writes PSS into
    `rss_peak_bytes` and the RSS sum into `rss_sum_peak_bytes`. Files with neither carry RSS only.
    """
    if "pss_peak_bytes" in target:
        return target["rss_peak_bytes"], target["pss_peak_bytes"]
    if "rss_sum_peak_bytes" in target:
        return target["rss_sum_peak_bytes"], target["rss_peak_bytes"]
    return target["rss_peak_bytes"], None


def row(path, run):
    step = at_max(run)
    out = {
        "file": os.path.basename(path),
        "max_concurrent": run["conclusion"].get("max_concurrent"),
        "first_failure": run["conclusion"].get("first_failure"),
        "limit": run["conclusion"].get("limit"),
        "failure_reason": first_failure_reason(run),
    }
    if step:
        cores = step["target"]["cpu_cores"]
        rss, pss = memory_bytes(step["target"])
        out.update(
            p95_v2v_ms=round(step["v2v"]["p95_us"] / 1000),
            p50_v2v_ms=round(step["v2v"]["p50_us"] / 1000),
            server_cpu_cores=round(cores, 2),
            mcores_per_call=round(1000 * cores / step["concurrent"], 2),
            rss_peak_mb=round(rss / 1e6),
            generator_cpu_cores=round(step["client"]["cpu_cores"], 2),
            max_send_lateness_ms=round(step["max_send_lateness_us"] / 1000),
        )
        if pss is not None:
            out["pss_peak_mb"] = round(pss / 1e6)
    return out


def main():
    results, date = sys.argv[1], sys.argv[2]
    systems = sorted(
        {
            re.match(r"capacity-(.+)-linux-" + re.escape(date) + r"-run\d+\.json", os.path.basename(p)).group(1)
            for p in glob.glob(os.path.join(results, f"capacity-*-linux-{date}-run*.json"))
        }
    )
    for system in systems:
        paths = sorted(glob.glob(os.path.join(results, f"capacity-{system}-linux-{date}-run*.json")))
        runs = [(p, json.load(open(p))) for p in paths]
        rows = [row(p, r) for p, r in runs]
        caps = [r["max_concurrent"] or 0 for r in rows]
        summary = {
            "schema": "brisvox-bench/capacity-summary/1",
            "system": system,
            "date": date,
            "label": runs[0][1]["label"],
            "environment": runs[0][1]["environment"],
            "note": re.sub(r"^run \d+ of \d+; ", "", runs[0][1]["note"]),
            "runs": rows,
            "median_max_concurrent": statistics.median(caps),
            "min_max_concurrent": min(caps),
            "max_max_concurrent": max(caps),
        }
        with open(os.path.join(results, f"capacity-{system}-linux-{date}.json"), "w") as f:
            json.dump(summary, f, indent=2)
            f.write("\n")
        lines = [
            f"# Capacity summary: {system}, Linux, {date}",
            "",
            f"- {summary['label']}",
            f"- {summary['note']}",
            f"- {len(rows)} run(s), per-run files `capacity-{system}-linux-{date}-run<N>.{{md,json}}`; "
            "SLO p95 V2V <= floor 950 ms + 250 ms, no failed call, no lost turn",
            f"- **capacity (max concurrent calls within the SLO): median {summary['median_max_concurrent']:g}, "
            f"range {summary['min_max_concurrent']}-{summary['max_max_concurrent']}**",
            "",
            "| run | max concurrent | first failure | p95 V2V at max (ms) | server CPU (cores) | CPU per call (mcores) | RSS sum peak (MB) | PSS peak (MB) | generator CPU (cores) | send lateness (ms) |",
            "|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
        ]
        for i, r in enumerate(rows, 1):
            lines.append(
                f"| {i} | {r['max_concurrent']} | {r['first_failure']} | {r.get('p95_v2v_ms', '-')} | "
                f"{r.get('server_cpu_cores', '-')} | {r.get('mcores_per_call', '-')} | {r.get('rss_peak_mb', '-')} | "
                f"{r.get('pss_peak_mb', '-')} | {r.get('generator_cpu_cores', '-')} | {r.get('max_send_lateness_ms', '-')} |"
            )
        lines += ["", "First failing step of each run:", ""]
        for i, r in enumerate(rows, 1):
            lines.append(f"- run {i}: {r['first_failure']} calls ({r['limit']}): {r['failure_reason'] or 'n/a'}")
        lines.append("")
        with open(os.path.join(results, f"capacity-{system}-linux-{date}.md"), "w") as f:
            f.write("\n".join(lines))
        print(f"{system}: runs {caps}, median {summary['median_max_concurrent']:g}")


if __name__ == "__main__":
    main()
