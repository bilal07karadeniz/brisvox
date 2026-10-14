#!/usr/bin/env python3
"""Generate the README / book benchmark charts (assets/charts/*.svg) from the committed results.

Usage:
    python3 scripts/gen_charts.py           # (re)write assets/charts/*.svg
    python3 scripts/gen_charts.py --check   # exit 1 if a chart is stale, missing or unexpected

Standard library only, deterministic (no timestamps), so the staleness check is exact.
verify.sh runs --check. Every number in a chart is read from a file in bench/results/; the chart's
own caption names the files. Which file feeds which chart:

  capacity-models.svg              capacity, real models      capacity-<system>-...-models-linux-<date>.json
  capacity-framework.svg           capacity, framework only   capacity-<system>-linux-<date>.json
  memory-models.svg                PSS at the maximum load    the same files as capacity-models.svg
  latency-vs-calls-models.svg      p50/p95 V2V per step       the run files named by those summaries
  latency-vs-calls-framework.svg   p50/p95 V2V per step       the run files named by capacity-framework
  v2v-live.svg                     live V2V, real providers   v2v-live-<system>[-tr]-<platform>-<date>.json
  engine-overhead.svg              engine time per stage      v2v-sim.json (engine_stages)

A chart is skipped (and its file must not exist) when none of its inputs exist.

The measured rows of the README comparison table (between the `measured:begin` / `measured:end`
comments) are generated from the same files; a system without files shows "not measured".

Adding a system: write its result files with the same schema under these names and the next run of
this script adds it to every chart, no code change (the colour is fixed per system in COLORS):

  capacity, real models   capacity-livekit-models-linux-<date>.json    (+ the -run<N>.json files)
  capacity, framework     capacity-livekit-linux-<date>.json           (+ the -run<N>.json files)
  live V2V                v2v-live-livekit-windows-<date>.json, v2v-live-livekit-tr-windows-<date>.json

(`bench/capacity_summary.py` writes the capacity pair; `scripts/bench_live_report.py` the live
files.) When several dates exist for a system, the latest date wins.
"""

from __future__ import annotations

import json
import math
import re
import statistics
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "bench" / "results"
OUT = ROOT / "assets" / "charts"

# --- which result files feed the capacity charts ---------------------------------------------
# (system id, legend label, glob in bench/results). Only files with the capacity-summary schema
# match (the -run<N> files share the glob and are skipped); the latest date wins.
MODELS_SERIES = [
    ("brisvox", "brisvox", "capacity-brisvox-q-turnexec-r*-models-linux-*.json"),
    ("pipecat", "Pipecat 1.12 (tuned)", "capacity-pipecat-q-tuned-models-linux-*.json"),
    ("livekit", "LiveKit Agents 1.8 (tuned)", "capacity-livekit-models-linux-*.json"),
    ("livekit-default", "LiveKit Agents 1.8 (defaults)", "capacity-livekit-default-models-linux-*.json"),
]
FRAMEWORK_SERIES = [
    ("brisvox", "brisvox", "capacity-brisvox-linux-*.json"),
    ("pipecat", "Pipecat 1.12 (tuned)", "capacity-pipecat-tuned-linux-*.json"),
    ("pipecat-default", "Pipecat 1.12 (defaults)", "capacity-pipecat-linux-*.json"),
    ("livekit", "LiveKit Agents", "capacity-livekit-linux-*.json"),
]
LIVE_SYSTEMS = {"brisvox": "brisvox", "pipecat": "Pipecat 1.12", "livekit": "LiveKit Agents"}
# The charts and the README show the English comparison; the Turkish files stay in bench/results.
LIVE_LANGS = [("en", "English")]
ENGINE_SIM = "v2v-sim.json"

# --- palette: the validated reference categorical order, fixed per entity ---------------------
# id: (light, dark). Colour follows the system, never its rank in a chart.
COLORS = {
    "brisvox": ("#2a78d6", "#3987e5"),  # blue
    "pipecat": ("#eb6834", "#d95926"),  # orange
    "livekit": ("#1baf7a", "#199e70"),  # aqua
    "pipecat-default": ("#eda100", "#c98500"),  # yellow
}
FALLBACK = [("#e87ba4", "#d55181"), ("#4a3aa7", "#9085e9"), ("#e34948", "#e66767")]

W = 720
ML, MR = 72, 28
FONT = 13


def esc(s: str) -> str:
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")


def f1(x: float) -> str:
    return f"{x:.1f}".rstrip("0").rstrip(".")


def read_json(p: Path) -> dict:
    return json.loads(p.read_text(encoding="utf-8"))


def wrap(text: str, width: int) -> list[str]:
    lines, cur = [], ""
    for word in text.split():
        if cur and len(cur) + 1 + len(word) > width:
            lines.append(cur)
            cur = word
        else:
            cur = f"{cur} {word}".strip()
    if cur:
        lines.append(cur)
    return lines


def text_w(s: str, size: int = FONT) -> float:
    return len(s) * size * 0.56


# --- data loading ----------------------------------------------------------------------------
def summaries(pattern: str) -> list[tuple[Path, dict]]:
    """Capacity-summary files matching the glob, latest date only, in file-name order."""
    found = []
    for p in sorted(RESULTS.glob(pattern)):
        try:
            d = read_json(p)
        except (OSError, ValueError):
            continue
        if d.get("schema") == "brisvox-bench/capacity-summary/1" and d.get("runs"):
            found.append((p, d))
    if not found:
        return []
    latest = max(d.get("date", "") for _, d in found)
    return [(p, d) for p, d in found if d.get("date", "") == latest]


def load_capacity(series_def) -> list[dict]:
    """One dict per system that has data: runs (list of per-run summary rows), files, run docs."""
    out = []
    for sid, label, pattern in series_def:
        sums = summaries(pattern)
        if not sums:
            continue
        runs, files = [], []
        for p, d in sums:
            files.append(p.name)
            for r in d["runs"]:
                # A run in which no step passed has no maximum: it carried 0 calls within the SLO.
                if r.get("max_concurrent") is None:
                    r = {**r, "max_concurrent": 0}
                rp = RESULTS / r["file"]
                doc = read_json(rp) if rp.is_file() else None
                runs.append({"row": r, "doc": doc})
        out.append({"id": sid, "label": label, "files": files, "runs": runs})
    return out


def container_text(series: list[dict]) -> str:
    for s in series:
        for r in s["runs"]:
            note = (r["doc"] or {}).get("note", "")
            cpus = re.search(r"--cpus=(\d+)", note)
            mem = re.search(r"--memory=(\d+)g", note)
            if cpus and mem:
                return f"{cpus.group(1)}-CPU / {mem.group(1)} GiB container"
    return "equal container"


def slo_text(series: list[dict]) -> tuple[str, float]:
    for s in series:
        for r in s["runs"]:
            st = (r["doc"] or {}).get("settings", {})
            if "slo_limit_ms" in st:
                return f"p95 voice-to-voice <= {st['slo_limit_ms']} ms", float(st["slo_limit_ms"])
    return "p95 voice-to-voice within the SLO", 0.0


def color_css(ids: list[str]) -> str:
    css, extra = [], iter(FALLBACK)
    for sid in ids:
        light, dark = COLORS.get(sid) or next(extra, ("#888888", "#aaaaaa"))
        css.append(f".f-{sid}{{fill:{light}}}.s-{sid}{{stroke:{light}}}")
        css.append(f"@media (prefers-color-scheme:dark){{.f-{sid}{{fill:{dark}}}.s-{sid}{{stroke:{dark}}}}}")
    return "\n".join(css)


BASE_CSS = """text{font-family:system-ui,-apple-system,"Segoe UI",Roboto,"Helvetica Neue",Arial,sans-serif}
.bg{fill:#fcfcfb}.t1{fill:#0b0b0b}.t2{fill:#52514e}.grid{stroke:#e3e2de}.axis{stroke:#8a8983}
.ring{stroke:#fcfcfb}.surf{fill:#fcfcfb}.ink{fill:#0b0b0b}.slo{stroke:#d03b3b}.slot{fill:#d03b3b}
@media (prefers-color-scheme:dark){.bg{fill:#1a1a19}.t1{fill:#ffffff}.t2{fill:#c3c2b7}.grid{stroke:#383835}
.axis{stroke:#7b7a74}.ring{stroke:#1a1a19}.surf{fill:#1a1a19}.ink{fill:#ffffff}.slo{stroke:#ec835a}.slot{fill:#ec835a}}"""


class Doc:
    """A tiny SVG builder: header block (title, subtitle, legend), body, source caption."""

    def __init__(self, uid: str, title: str, subtitle: str, desc: str, ids: list[str]):
        self.uid, self.title, self.desc, self.ids = uid, title, desc, ids
        self.parts: list[str] = []
        self.y = 30
        self.add(f'<text x="{ML - 48}" y="{self.y}" class="t1" font-size="17" font-weight="700">{esc(title)}</text>')
        for line in wrap(subtitle, 104):
            self.y += 19
            self.add(f'<text x="{ML - 48}" y="{self.y}" class="t2" font-size="{FONT}">{esc(line)}</text>')
        self.y += 10

    def add(self, s: str) -> None:
        self.parts.append(s)

    def legend(self, items: list[tuple[str, str]], extra: str = "") -> None:
        """items: (series id, label). Swatch + text, laid out left to right, wrapping."""
        x = ML - 48
        self.y += 18
        for sid, label in items:
            w = 16 + text_w(label) + 22
            if x + w > W - MR:
                x = ML - 48
                self.y += 20
            self.add(f'<rect x="{x}" y="{self.y - 10}" width="12" height="12" rx="3" class="f-{sid}"/>')
            self.add(f'<text x="{x + 18}" y="{self.y}" class="t1" font-size="{FONT}">{esc(label)}</text>')
            x += w
        if extra:
            self.add(f'<text x="{x}" y="{self.y}" class="t2" font-size="{FONT}">{esc(extra)}</text>')
        self.y += 12

    def finish(self, source: str) -> str:
        self.y += 8
        for line in wrap("Source: " + source, 108):
            self.y += 16
            self.add(f'<text x="{ML - 48}" y="{self.y}" class="t2" font-size="12">{esc(line)}</text>')
        h = self.y + 14
        head = (
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {h}" width="{W}" height="{h}" '
            f'role="img" aria-labelledby="{self.uid}-t {self.uid}-d">\n'
            f'<title id="{self.uid}-t">{esc(self.title)}</title>\n'
            f'<desc id="{self.uid}-d">{esc(self.desc)}</desc>\n'
            f"<style>\n{BASE_CSS}\n{color_css(self.ids)}\n</style>\n"
            f'<rect class="bg" x="0" y="0" width="{W}" height="{h}" rx="10"/>\n'
        )
        return head + "\n".join(self.parts) + "\n</svg>\n"


def nice_ticks(vmax: float, n: int = 5) -> list[float]:
    if vmax <= 0:
        return [0.0, 1.0]
    raw = vmax / n
    mag = 10 ** math.floor(math.log10(raw))
    step = next(m * mag for m in (1, 2, 2.5, 5, 10) if m * mag >= raw)
    ticks, v = [], 0.0
    while v < vmax + step * 0.999:
        ticks.append(v)
        v += step
        if len(ticks) > 12:
            break
    return ticks


def tick_label(v: float) -> str:
    return f"{int(v)}" if float(v).is_integer() else f1(v)


def bar_path(x: float, y: float, w: float, h: float, r: float = 4) -> str:
    """A bar standing on the baseline with a rounded data end (top)."""
    r = min(r, w / 2, h)
    return (
        f"M{f1(x)} {f1(y + h)}V{f1(y + r)}Q{f1(x)} {f1(y)} {f1(x + r)} {f1(y)}H{f1(x + w - r)}"
        f"Q{f1(x + w)} {f1(y)} {f1(x + w)} {f1(y + r)}V{f1(y + h)}Z"
    )


def grouped_bars(doc: Doc, groups: list[dict], ytitle: str, plot_h: int = 250) -> None:
    """groups: {label, sub: [lines], bars: [{series, value, text, runs}]}. Linear axis from 0."""
    vmax = max((b["value"] for g in groups for b in g["bars"]), default=1)
    vmax = max([vmax] + [r for g in groups for b in g["bars"] for r in b.get("runs", [])])
    ticks = nice_ticks(vmax * 1.12)
    top_v = ticks[-1]
    doc.y += 12
    top = doc.y
    base = top + plot_h
    pw = W - ML - MR

    def Y(v: float) -> float:
        return base - v / top_v * plot_h

    for t in ticks:
        y = Y(t)
        cls = "axis" if t == 0 else "grid"
        doc.add(f'<line x1="{ML}" x2="{W - MR}" y1="{f1(y)}" y2="{f1(y)}" class="{cls}" stroke-width="1"/>')
        doc.add(f'<text x="{ML - 8}" y="{f1(y + 4)}" class="t2" font-size="12" text-anchor="end">{tick_label(t)}</text>')
    doc.add(
        f'<text transform="translate(16 {f1(top + plot_h / 2)}) rotate(-90)" class="t2" font-size="12" '
        f'text-anchor="middle">{esc(ytitle)}</text>'
    )
    slot = pw / len(groups)
    max_lines = 0
    for gi, g in enumerate(groups):
        nb = len(g["bars"])
        bw = min(84.0, (slot * 0.82 - (nb - 1) * 8) / nb)
        total = nb * bw + (nb - 1) * 8
        x0 = ML + slot * gi + (slot - total) / 2
        for bi, b in enumerate(g["bars"]):
            x = x0 + bi * (bw + 8)
            h = max(b["value"] / top_v * plot_h, 1.5)
            doc.add(f'<path d="{bar_path(x, base - h, bw, h)}" class="f-{b["series"]}"/>')
            runs = b.get("runs", [])
            high = b["value"]
            if len(runs) > 1:
                for ri, r in enumerate(runs):
                    cx = x + bw / 2 + (ri - (len(runs) - 1) / 2) * 14
                    doc.add(f'<circle cx="{f1(cx)}" cy="{f1(Y(r))}" r="4.5" class="ink ring" stroke-width="2"/>')
                high = max(runs)
            doc.add(
                f'<text x="{f1(x + bw / 2)}" y="{f1(Y(high) - 11)}" class="t1" font-size="14" font-weight="700" '
                f'text-anchor="middle">{esc(b["text"])}</text>'
            )
        cx = ML + slot * gi + slot / 2
        lines = [g["label"]] + g.get("sub", [])
        max_lines = max(max_lines, len(lines))
        for li, line in enumerate(lines):
            cls, wt = ("t1", "600") if li == 0 else ("t2", "400")
            doc.add(
                f'<text x="{f1(cx)}" y="{f1(base + 20 + li * 16)}" class="{cls}" font-size="{FONT if li == 0 else 12}" '
                f'font-weight="{wt}" text-anchor="middle">{esc(line)}</text>'
            )
    doc.y = base + 20 + max_lines * 16 + 4


def fmt_mem(mb: float) -> str:
    return f"{mb / 1000:.1f} GB" if mb >= 1000 else f"{mb:.0f} MB"


# --- the charts ------------------------------------------------------------------------------
def files_text(names: list[str]) -> str:
    return names[0] and (f"bench/results/{names[0]}" if len(names) == 1 else "bench/results/{" + ", ".join(names) + "}")


def srcs(series: list[dict], extra: str = "") -> str:
    names = []
    for s in series:
        names.extend(s["files"])
    return files_text(names) + extra


def ratio_text(series: list[dict], key=lambda s: statistics.median(r["row"]["max_concurrent"] for r in s["runs"])) -> str:
    me = next((s for s in series if s["id"] == "brisvox"), None)
    if me is None:
        return ""
    parts = []
    for s in series:
        if s is me:
            continue
        o = key(s)
        if o:
            parts.append(f"{key(me) / o:.0f}x {s['label']}")
    return "brisvox carries about " + " and ".join(parts) + "." if parts else ""


def chart_capacity(series: list[dict], uid: str, title: str, what: str) -> str:
    cont = container_text(series)
    slo, _ = slo_text(series)
    groups = []
    for s in series:
        vals = [r["row"]["max_concurrent"] for r in s["runs"]]
        med = statistics.median(vals)
        groups.append(
            {
                "label": s["label"],
                "sub": ["runs: " + ", ".join(str(v) for v in vals)],
                "bars": [{"series": s["id"], "value": med, "text": f"{med:.0f}", "runs": vals}],
            }
        )
    ratio = ratio_text(series)
    sub = f"Concurrent calls within the SLO ({slo}), median of the runs (dots), one {cont}, mock providers. {ratio}"
    desc = (
        f"Bar chart, {what}: "
        + "; ".join(f"{g['label']} {g['bars'][0]['text']} calls ({g['sub'][0]})" for g in groups)
        + f". {ratio}"
    )
    doc = Doc(uid, title, sub, desc, [s["id"] for s in series])
    grouped_bars(doc, groups, "concurrent calls")
    return doc.finish(srcs(series) + ". Linux (WSL2, Docker), per-system run lists as in the file names.")


def mem_mb(row: dict) -> float | None:
    """Memory of a run at its maximum, MB, the same way for every system: PSS (proportional set size:
    a page shared by N processes counts 1/N in each) summed over the server's processes.

    The 2026-10-14 Pipecat and LiveKit files carry `pss_peak_mb`. The brisvox files of 2026-10-13
    are one process, which shares nothing, so there its RSS is its PSS; the PSS cross-check run
    (capacity-brisvox-q-pss-crosscheck-models-linux-*) measures both for brisvox."""
    return row.get("pss_peak_mb", row.get("rss_peak_mb"))


def chart_memory(series: list[dict]) -> str:
    cont = container_text(series)
    groups = []
    for s in series:
        if not any(r["row"]["max_concurrent"] for r in s["runs"]):
            continue  # nothing carried within the SLO: no "memory at the maximum"
        passed = [r["row"] for r in s["runs"] if r["row"]["max_concurrent"] and mem_mb(r["row"])]
        rss = [mem_mb(row) for row in passed]
        calls = [row["max_concurrent"] for row in passed]
        med = statistics.median(rss)
        per = statistics.median(m / c for m, c in zip(rss, calls))
        groups.append(
            {
                "label": s["label"],
                "sub": [f"{statistics.median(calls):.0f} calls", f"{per:.1f} MB per call"],
                "bars": [{"series": s["id"], "value": med, "text": fmt_mem(med)}],
            }
        )
    sub = (
        f"Memory of the whole server (all its processes) at the highest load that met the SLO, "
        f"median of the runs, one {cont}, real VAD and turn models. Counted alike for all three: PSS (proportional set size, "
        f"a page shared by several processes is split between them) summed over the server's processes; brisvox is one process, "
        f"where PSS and RSS agree within 1 %. Per call = memory / calls, idle baseline included."
    )
    desc = "Bar chart, memory at maximum load with the real models: " + "; ".join(
        f"{g['label']} {g['bars'][0]['text']} at {g['sub'][0]}, {g['sub'][1]}" for g in groups
    )
    doc = Doc("memory-models", "Memory at maximum load", sub, desc, [s["id"] for s in series])
    grouped_bars(doc, groups, "proportional memory, PSS (MB)")
    return doc.finish(srcs(series) + " (pss_peak_mb of each run; brisvox: rss_peak_mb of its single process, = PSS).")


def run_steps(s: dict) -> list[dict]:
    """The ramp of the run whose maximum is the (low) median: p50/p95 V2V per concurrency step."""
    runs = [r for r in s["runs"] if r["doc"]]
    if not runs:
        return []
    med = statistics.median_low(r["row"]["max_concurrent"] for r in runs)
    pick = next(r for r in runs if r["row"]["max_concurrent"] == med)
    pts = []
    for st in pick["doc"].get("steps", []):
        v = st.get("v2v")
        if v and v.get("count"):
            pts.append(
                {
                    "n": st["concurrent"],
                    "p50": v["p50_us"] / 1000,
                    "p95": v["p95_us"] / 1000,
                    "ok": st.get("verdict") == "pass",
                }
            )
    s["_ramp_file"] = pick["row"]["file"]
    return sorted(pts, key=lambda p: p["n"])  # the bisection steps come after the ramp in the file


def chart_latency(series: list[dict], uid: str, title: str) -> str | None:
    data = []
    for s in series:
        pts = run_steps(s)
        if len(pts) >= 2:
            data.append((s, pts))
    if not data:
        return None
    cont = container_text(series)
    slo_s, slo = slo_text(series)
    ymax_cap = max(slo * 1.9, 1000)
    allp = [p for _, pts in data for p in pts]
    y_lo = max(0.0, math.floor((min(p["p50"] for p in allp) - 60) / 100) * 100)
    y_hi = min(max(p["p95"] for p in allp), ymax_cap)
    y_hi = max(y_hi, slo * 1.15)
    y_hi = math.ceil(y_hi / 200) * 200
    # A linear axis: the distance between the systems' capacities is the point of the chart (a log
    # axis drew 20 calls next to 325).
    xmax = max(p["n"] for p in allp)
    xstep = next(t for t in (5, 10, 20, 25, 50, 100, 200, 250, 500, 1000, 2000) if xmax / t <= 7)
    xtop = math.ceil(xmax / xstep) * xstep
    sub = (
        f"Voice-to-voice latency at each load step of the capacity ramp (one {cont}, mock providers, {slo_s}). "
        "Solid: p95, dashed: p50. Hollow marker: the step missed the SLO, by latency or by failed calls (the p95 is over the "
        "turns that were answered, so a step that lost calls can show a low p95). A step beyond the axis is drawn at the top."
    )
    desc = "Line chart, latency versus concurrent calls (linear axis): " + "; ".join(
        f"{s['label']} from {pts[0]['n']} to {pts[-1]['n']} calls, p95 {pts[0]['p95']:.0f} ms to {pts[-1]['p95']:.0f} ms"
        for s, pts in data
    )
    doc = Doc(uid, title, sub, desc, [s["id"] for s, _ in data])
    doc.legend([(s["id"], s["label"]) for s, _ in data])
    doc.y += 8
    top, plot_h = doc.y, 270
    base = top + plot_h
    pw = W - ML - MR - 112  # room for direct labels at the right

    def X(n: float) -> float:
        return ML + n / xtop * pw

    def Y(v: float) -> float:
        return base - (min(v, y_hi) - y_lo) / (y_hi - y_lo) * plot_h

    step = 200 if y_hi - y_lo > 800 else 100
    v = y_lo
    while v <= y_hi + 0.1:
        y = Y(v)
        doc.add(f'<line x1="{ML}" x2="{ML + pw}" y1="{f1(y)}" y2="{f1(y)}" class="grid" stroke-width="1"/>')
        doc.add(f'<text x="{ML - 8}" y="{f1(y + 4)}" class="t2" font-size="12" text-anchor="end">{v:.0f}</text>')
        v += step
    doc.add(f'<line x1="{ML}" x2="{ML + pw}" y1="{base}" y2="{base}" class="axis" stroke-width="1"/>')
    for t in range(0, int(xtop) + 1, xstep):
        doc.add(f'<text x="{f1(X(t))}" y="{base + 18}" class="t2" font-size="12" text-anchor="middle">{t}</text>')
        doc.add(f'<line x1="{f1(X(t))}" x2="{f1(X(t))}" y1="{base}" y2="{base + 4}" class="axis" stroke-width="1"/>')
    doc.add(
        f'<text x="{f1(ML + pw / 2)}" y="{base + 38}" class="t2" font-size="12" text-anchor="middle">'
        "concurrent calls</text>"
    )
    doc.add(
        f'<text transform="translate(16 {f1(top + plot_h / 2)}) rotate(-90)" class="t2" font-size="12" '
        'text-anchor="middle">voice-to-voice latency (ms)</text>'
    )
    if slo:
        ys = Y(slo)
        doc.add(f'<line x1="{ML}" x2="{ML + pw}" y1="{f1(ys)}" y2="{f1(ys)}" class="slo" stroke-width="1.5" stroke-dasharray="2 4"/>')
        doc.add(f'<text x="{ML + pw + 6}" y="{f1(ys + 4)}" class="slot" font-size="12">SLO {slo:.0f} ms</text>')
    label_ys: list[float] = []
    for s, pts in data:
        sid = s["id"]
        for key, dash, sw in (("p50", "5 4", 1.5), ("p95", "", 2.5)):
            d = " ".join(f"{'M' if i == 0 else 'L'}{f1(X(p['n']))} {f1(Y(p[key]))}" for i, p in enumerate(pts))
            da = f' stroke-dasharray="{dash}"' if dash else ""
            doc.add(f'<path d="{d}" class="s-{sid}" fill="none" stroke-width="{sw}"{da} stroke-linejoin="round"/>')
        for p in pts:
            x, y = X(p["n"]), Y(p["p95"])
            clipped = p["p95"] > y_hi
            if clipped:
                doc.add(
                    f'<path d="M{f1(x)} {f1(y - 5)}L{f1(x + 5)} {f1(y + 4)}H{f1(x - 5)}Z" class="f-{sid} ring" stroke-width="1.5"/>'
                )
            elif p["ok"]:
                doc.add(f'<circle cx="{f1(x)}" cy="{f1(y)}" r="4.5" class="f-{sid} ring" stroke-width="2"/>')
            else:
                doc.add(f'<circle cx="{f1(x)}" cy="{f1(y)}" r="4.5" class="surf s-{sid}" stroke-width="2"/>')
        last = pts[-1]
        passed = [p["n"] for p in pts if p["ok"]]
        cap = f": {max(passed)} calls" if passed else ""
        ly = Y(last["p95"])
        while any(abs(ly - o) < 15 for o in label_ys):
            ly += 15
        label_ys.append(ly)
        doc.add(
            f'<text x="{f1(X(last["n"]) + 9)}" y="{f1(ly + 4)}" class="t1" font-size="12" font-weight="600">'
            f'{esc(s["label"].replace(" 1.12", "") + cap)}</text>'
        )
    doc.y = base + 44
    files = [s["_ramp_file"] for s, _ in data]
    return doc.finish(files_text(files) + " (steps[].v2v of the run with the median maximum).")


def live_docs() -> dict[tuple[str, str], tuple[Path, dict]]:
    """(system, lang) -> (path, doc) for the latest date of v2v-live-<system>[-tr]-<platform>-<date>.json."""
    found: dict[tuple[str, str], tuple[str, Path, dict]] = {}
    for p in sorted(RESULTS.glob("v2v-live-*.json")):
        try:
            d = read_json(p)
        except (OSError, ValueError):
            continue
        if d.get("schema") != "brisvox-live-v2v/1" or not d.get("v2v", {}).get("pooled"):
            continue
        key = (d["system"], d.get("lang", "en"))
        if key not in found or d.get("date", "") >= found[key][0]:
            found[key] = (d.get("date", ""), p, d)
    return {k: (p, d) for k, (_, p, d) in found.items()}


TTS_LABELS = {"cartesia": "Cartesia", "deepgram_aura": "Deepgram Aura"}


def live_label(doc: dict) -> str:
    """How a live run is described next to its numbers: concurrency, turns, and the TTS voice when the
    run recorded them (the 2026-10-13 files did not: one call at a time, Cartesia)."""
    conc = doc.get("concurrency", 1)
    n = doc["v2v"]["pooled"]["all"]["n"]
    parts = []
    if conc > 1:
        parts.append(f"{conc} concurrent calls")
    parts.append(f"{n} turns")
    if doc.get("tts"):
        parts.append(f"{TTS_LABELS.get(doc['tts'], doc['tts'])} voice")
    return ", ".join(parts)


def chart_live() -> str | None:
    langs = {lang for lang, _ in LIVE_LANGS}
    docs = {k: v for k, v in live_docs().items() if k[1] in langs}
    if not docs:
        return None
    systems = [s for s in LIVE_SYSTEMS if any(k[0] == s for k in docs)]
    systems += sorted({k[0] for k in docs} - set(LIVE_SYSTEMS))
    groups = []
    for lang, lname in LIVE_LANGS:
        have = [s for s in systems if (s, lang) in docs]
        if len(have) < 1:
            continue
        for pct, pname in (("p50", "median (p50)"), ("p95", "p95")):
            bars, ns = [], []
            for s in have:
                a = docs[(s, lang)][1]["v2v"]["pooled"]["all"]
                bars.append({"series": s, "value": a[pct], "text": f"{a[pct] / 1000:.2f} s"})
                ns.append(a["n"])
            groups.append({"label": f"{lname} {pname}", "sub": ["n = " + " / ".join(str(n) for n in ns)], "bars": bars})
    if not groups:
        return None
    first = next(iter(docs.values()))[1]
    prov = first["environment"]["providers"]
    ptxt = f"{prov['stt']}, {prov['llm'].split(' (')[0]}, {prov['tts']}"
    labels = sorted({live_label(d) for _, d in docs.values()})
    sub = (
        f"Voice-to-voice latency on real providers ({ptxt}; {' / '.join(labels)}), the same agent file for every system, "
        "one machine (Windows, public internet), the systems run in alternating blocks, never at the same time. "
        "Small samples: n turns per bar in order of the legend."
    )
    desc = "Grouped bar chart, live voice-to-voice latency: " + "; ".join(
        f"{g['label']}: " + ", ".join(f"{LIVE_SYSTEMS.get(b['series'], b['series'])} {b['text']}" for b in g["bars"]) for g in groups
    )
    ids = [s for s in systems]
    doc = Doc("v2v-live", "Voice-to-voice on real providers", sub, desc, ids)
    doc.legend([(s, LIVE_SYSTEMS.get(s, s)) for s in systems])
    grouped_bars(doc, groups, "V2V latency (ms)", plot_h=230)
    files = sorted({p.name for p, _ in docs.values()})
    return doc.finish(files_text(files) + " (v2v.pooled.all). Not a Linux run; absolute values carry noise.")


def fmt_us(us: float) -> str:
    if us < 1000:
        return f"{us:.0f} µs"
    return f"{us / 1000:.2f}".rstrip("0").rstrip(".") + " ms"


STAGE_NAMES = {
    "vad_stop_to_finalize": "speech stop to STT finalize",
    "turn_end_to_llm_request": "end of turn to LLM request",
    "first_token_to_tts_text": "first LLM token to TTS text",
    "tts_audio_to_transport": "TTS audio to transport",
    "interrupt_to_clear": "barge-in to audio cleared",
}


def chart_engine() -> str | None:
    p = RESULTS / ENGINE_SIM
    if not p.is_file():
        return None
    d = read_json(p)
    stages = [
        s
        for s in d.get("engine_stages", {}).get("stages", [])
        if s["name"] in STAGE_NAMES and s.get("provider_us", 0) == 0
    ]
    if not stages:
        return None
    stages.sort(key=lambda s: -s["p95_us"])
    runs = d["overall"]["turns"]
    sub = (
        "The engine's own time in each stage of a turn (provider time excluded), p50 and p95 over "
        f"{stages[0]['count']} measured spans, {runs} turns over the websocket, mock providers. Log axis; the red bar is the budget at p95."
    )
    desc = "Dot plot, engine time per stage (log scale): " + "; ".join(
        f"{STAGE_NAMES[s['name']]} p50 {fmt_us(s['p50_us'])}, p95 {fmt_us(s['p95_us'])}"
        + (f", budget {fmt_us(s['budget_p95_us'])}" if s.get("budget_p95_us") else "")
        for s in stages
    )
    doc = Doc("engine-overhead", "Engine overhead per stage", sub, desc, ["brisvox"])
    doc.y += 8
    left = 232
    pw = W - left - MR
    top = doc.y + 8
    row = 46
    base = top + row * len(stages)
    lo, hi = 1.0, 10000.0  # microseconds

    def X(us: float) -> float:
        return left + (math.log10(max(us, lo)) - math.log10(lo)) / (math.log10(hi) - math.log10(lo)) * pw

    for t, lab in ((1, "1 µs"), (10, "10 µs"), (100, "100 µs"), (1000, "1 ms"), (10000, "10 ms")):
        doc.add(f'<line x1="{f1(X(t))}" x2="{f1(X(t))}" y1="{top - 6}" y2="{base}" class="grid" stroke-width="1"/>')
        doc.add(f'<text x="{f1(X(t))}" y="{base + 18}" class="t2" font-size="12" text-anchor="middle">{lab}</text>')
    doc.add(f'<line x1="{left}" x2="{left + pw}" y1="{base}" y2="{base}" class="axis" stroke-width="1"/>')
    doc.add(
        f'<text x="{f1(left + pw / 2)}" y="{base + 38}" class="t2" font-size="12" text-anchor="middle">'
        "time spent in the stage (log scale)</text>"
    )
    for i, s in enumerate(stages):
        cy = top + row * i + row / 2
        doc.add(f'<text x="{left - 12}" y="{f1(cy + 4)}" class="t1" font-size="{FONT}" text-anchor="end">{esc(STAGE_NAMES[s["name"]])}</text>')
        x50, x95 = X(s["p50_us"]), X(s["p95_us"])
        doc.add(f'<line x1="{f1(x50)}" x2="{f1(x95)}" y1="{f1(cy)}" y2="{f1(cy)}" class="s-brisvox" stroke-width="3"/>')
        doc.add(f'<circle cx="{f1(x50)}" cy="{f1(cy)}" r="4.5" class="surf s-brisvox" stroke-width="2.5"/>')
        doc.add(f'<circle cx="{f1(x95)}" cy="{f1(cy)}" r="5" class="f-brisvox ring" stroke-width="2"/>')
        doc.add(
            f'<text x="{f1(x95 + 11)}" y="{f1(cy + 4)}" class="t1" font-size="12" font-weight="600">'
            f"p95 {fmt_us(s['p95_us'])}</text>"
        )
        b = s.get("budget_p95_us")
        if b:
            xb = X(b)
            doc.add(f'<line x1="{f1(xb)}" x2="{f1(xb)}" y1="{f1(cy - 13)}" y2="{f1(cy + 13)}" class="slo" stroke-width="3"/>')
            doc.add(f'<text x="{f1(xb)}" y="{f1(cy - 17)}" class="slot" font-size="12" text-anchor="middle">budget {fmt_us(b)}</text>')
    doc.add(f'<circle cx="{left}" cy="{base + 56}" r="4.5" class="surf s-brisvox" stroke-width="2.5"/>')
    doc.add(f'<text x="{left + 10}" y="{base + 60}" class="t2" font-size="12">p50</text>')
    doc.add(f'<circle cx="{left + 56}" cy="{base + 56}" r="5" class="f-brisvox ring" stroke-width="2"/>')
    doc.add(f'<text x="{left + 66}" y="{base + 60}" class="t2" font-size="12">p95</text>')
    doc.y = base + 62
    return doc.finish(f"bench/results/{ENGINE_SIM} (engine_stages). Windows x86_64 development machine, release build.")


# --- the measured rows of the README comparison table ----------------------------------------
README = ROOT / "README.md"
BEGIN = "<!-- measured:begin (generated by scripts/gen_charts.py from bench/results; do not edit) -->"
END = "<!-- measured:end -->"
PENDING = "not measured"
# table column -> the capacity series ids it shows (a system can have a tuned and a default run)
COLUMNS = [
    ("brisvox", ["brisvox"]),
    ("livekit", ["livekit", "livekit-default"]),
    ("pipecat", ["pipecat", "pipecat-default"]),
]


def tag_of(label: str) -> str:
    m = re.search(r"\(([^)]*)\)", label)
    return m.group(1) if m else ""


def cap_cell(series: list[dict], ids: list[str], what) -> str:
    found = [s for sid in ids for s in series if s["id"] == sid]
    parts = []
    for s in found:
        tag = tag_of(s["label"])
        text = what(s)
        if text is not None:
            parts.append(text + (f" {tag}" if tag and len(found) > 1 else ""))
    return "; ".join(parts) if parts else PENDING


def runs_cell(s: dict) -> str:
    vals = [r["row"]["max_concurrent"] for r in s["runs"]]
    return f"**{statistics.median(vals):.0f}** ({', '.join(str(v) for v in vals)})"


def mem_cell(s: dict) -> str | None:
    passed = [r["row"] for r in s["runs"] if r["row"]["max_concurrent"] and mem_mb(r["row"])]
    if not passed:
        return None  # no step passed: no maximum to report memory at
    rss = statistics.median(mem_mb(row) for row in passed)
    calls = statistics.median(row["max_concurrent"] for row in passed)
    return f"{fmt_mem(rss)} at {calls:.0f} calls"


def measured_table() -> str:
    models = load_capacity(MODELS_SERIES)
    frame = load_capacity(FRAMEWORK_SERIES)
    live = live_docs()
    cont = container_text(models or frame)
    rows = []

    def row(label: str, cells: list[str], source: str) -> None:
        rows.append(f"| {label} | " + " | ".join(cells) + f" | {source} |")

    row(
        f"Concurrent calls within the latency SLO, **real VAD and turn-detection models on every call** (Silero + Smart Turn; LiveKit: Silero + its own turn detector), one {cont}, mock providers (median, runs in brackets)",
        [cap_cell(models, ids, runs_cell) for _, ids in COLUMNS],
        "`bench/results/capacity-*-models-linux-*`",
    )
    row(
        f"Concurrent calls, **framework only** (energy VAD, fixed turn answer), same {cont}",
        [cap_cell(frame, ids, runs_cell) for _, ids in COLUMNS],
        "`bench/results/capacity-*-linux-*`",
    )
    row(
        "Memory at that maximum (real models; PSS summed over the server's processes for all three; brisvox is one process, PSS = RSS)",
        [cap_cell(models, ids, mem_cell) for _, ids in COLUMNS],
        "same files",
    )
    for lang, lname in LIVE_LANGS:
        cells = []
        for system, _ in COLUMNS:
            hit = live.get((system, lang))
            if hit:
                a = hit[1]["v2v"]["pooled"]["all"]
                cells.append(f"{a['p50'] / 1000:.2f} s / {a['p95'] / 1000:.2f} s ({a['n']} turns)")
            else:
                cells.append(PENDING)
        if any(c != PENDING for c in cells):
            label = next((live_label(live[(s, lang)][1]) for s, _ in COLUMNS if (s, lang) in live), "")
            row(f"Voice-to-voice on real providers, {lname}, p50 / p95 ({label}; Windows, public internet)", cells, f"`bench/results/v2v-live-*{'-tr' if lang == 'tr' else ''}-windows-*`")
    head = "| Measured here | brisvox | LiveKit Agents | Pipecat 1.12 | Source |\n|---|---|---|---|---|\n"
    return head + "\n".join(rows)


def readme_with(table: str) -> str:
    text = README.read_text(encoding="utf-8")
    if BEGIN not in text or END not in text:
        raise SystemExit(f"gen_charts: README.md lacks the {BEGIN!r} / {END!r} markers")
    pre, rest = text.split(BEGIN, 1)
    _, post = rest.split(END, 1)
    return f"{pre}{BEGIN}\n\n{table}\n\n{END}{post}"


def build_all() -> dict[str, str]:
    charts: dict[str, str | None] = {}
    models = load_capacity(MODELS_SERIES)
    frame = load_capacity(FRAMEWORK_SERIES)
    if models:
        charts["capacity-models.svg"] = chart_capacity(
            models,
            "capacity-models",
            "Concurrent calls per container, with the real models",
            "with the real Silero VAD and Smart Turn models on every call",
        )
        charts["memory-models.svg"] = chart_memory(models)
        charts["latency-vs-calls-models.svg"] = chart_latency(
            models, "latency-models", "Latency as the load grows, with the real models"
        )
    if frame:
        charts["capacity-framework.svg"] = chart_capacity(
            frame,
            "capacity-framework",
            "Concurrent calls per container, framework only",
            "framework only, energy VAD and a fixed turn answer, no models",
        )
        charts["latency-vs-calls-framework.svg"] = chart_latency(
            frame, "latency-framework", "Latency as the load grows, framework only"
        )
    charts["v2v-live.svg"] = chart_live()
    charts["engine-overhead.svg"] = chart_engine()
    return {k: v for k, v in charts.items() if v}


def main(argv: list[str]) -> int:
    charts = build_all()
    if not charts:
        print("gen_charts: no inputs found in bench/results", file=sys.stderr)
        return 1
    existing = {p.name for p in OUT.glob("*.svg")} if OUT.is_dir() else set()
    readme = readme_with(measured_table())
    if "--check" in argv:
        bad = []
        if README.read_text(encoding="utf-8") != readme:
            bad.append("README.md (measured table)")
        for name, text in charts.items():
            p = OUT / name
            if not p.is_file() or p.read_text(encoding="utf-8") != text:
                bad.append(name)
        extra = sorted(existing - set(charts))
        if bad or extra:
            if bad:
                print(f"gen_charts: stale or missing {', '.join(bad)}; run python3 scripts/gen_charts.py", file=sys.stderr)
            if extra:
                print(f"gen_charts: unexpected {', '.join(extra)} in assets/charts", file=sys.stderr)
            return 1
        print("gen_charts: ok")
        return 0
    if readme is not None and README.read_text(encoding="utf-8") != readme:
        README.write_text(readme, encoding="utf-8", newline="\n")
        print("gen_charts: updated the measured table in README.md")
    OUT.mkdir(parents=True, exist_ok=True)
    for name, text in charts.items():
        (OUT / name).write_text(text, encoding="utf-8", newline="\n")
        print(f"gen_charts: wrote assets/charts/{name} ({len(text)} bytes)")
    for name in sorted(existing - set(charts)):
        (OUT / name).unlink()
        print(f"gen_charts: removed assets/charts/{name} (no input data)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
