"""Render the measured Vite comparison; requires matplotlib."""
import json
from pathlib import Path
import sys

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

directory = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).parent
data = json.loads((directory / "measurements.json").read_text())
plt.rcParams.update({"font.family": "DejaVu Sans", "font.size": 11})
figure, axes = plt.subplots(3, 1, figsize=(9, 11))
figure.subplots_adjust(left=0.25, right=0.93, top=0.91, bottom=0.12, hspace=0.65)
figure.suptitle("Locked Vite build · Linux x86-64", fontsize=17, x=0.25, ha="left", y=0.975)
figure.text(0.25, 0.945, "Whole-process median · lower is faster", fontsize=11, color="#475569")

def bars(axis, labels, values, highlight, title, unit, maximum):
    axis.barh(labels, values, color=["#2563eb" if active else "#94a3b8" for active in highlight], edgecolor="none", height=0.62)
    axis.invert_yaxis()
    axis.set_xlim(0, maximum)
    axis.set_title(title, loc="left", fontsize=12, pad=12)
    axis.set_xlabel(unit)
    axis.xaxis.grid(True, color="#e2e8f0", linewidth=0.7)
    axis.set_axisbelow(True)
    axis.tick_params(axis="both", length=0, labelcolor="#334155", pad=7)
    for spine in axis.spines.values():
        spine.set_visible(False)
    for position, value in enumerate(values):
        axis.text(value + maximum * 0.015, position, f"{value:.1f}", va="center", fontsize=11, color="#0f172a")

labels = {"baseline": "Lumen default", "lumen": "Lumen profiled", "node": "Node 24.22.2", "bun": "Bun 1.3.10", "bunDefault": "Bun default cache¹"}
for axis, mode, title in [(axes[0], "warm", "Primed runtime cache"), (axes[1], "cold", "Empty runtime cache")]:
    names = ["baseline", "lumen", "node", "bun"] + (["bunDefault"] if mode == "cold" else [])
    bars(axis, [labels[name] for name in names], [data["modes"][mode]["medians"][name] for name in names], [name == "lumen" for name in names], title, "Build time (ms)", 300)

conditions = ["warm-baseline", "warm-candidate", "cold-baseline", "cold-candidate"]
bars(axes[2], ["Default · warm", "Profiled · warm", "Default · cold", "Profiled · cold"],
     [data["memory"]["summary"][name]["medianPeakRssKiB"] / 1024 for name in conditions],
     [False, True, False, True], "Memory tradeoff", "Peak RSS (MiB)", 200)
figure.text(0.03, 0.065, "200 measured processes per runtime/cache mode across two rotated sessions; identical build outputs.\n¹ Bun default retains its default cache and is a separate reference. RSS: eight processes per condition.\nGeneric x86-64, full LTO, mimalloc + compiler PGO; Vite and the app excluded from profile training.", fontsize=9.5, color="#475569", va="top", linespacing=1.5)
figure.savefig(directory / "comparison.png", dpi=180, facecolor="white")
figure.savefig(directory / "comparison.svg", facecolor="white")
svg = directory / "comparison.svg"
svg.write_text("\n".join(line.rstrip() for line in svg.read_text().splitlines()) + "\n")
plt.close(figure)
print(directory / "comparison.png")
