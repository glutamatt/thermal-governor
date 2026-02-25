#!/usr/bin/env python3
"""hw-plot.py — Visualize hardware characterization data.

Usage: .venv/bin/python hw-plot.py hw-data-*.csv
"""

import sys
import csv
from pathlib import Path
from collections import defaultdict

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def load_csv(path):
    rows = []
    with open(path) as f:
        for r in csv.DictReader(f):
            rows.append({
                "cap_mhz":       int(r["cap_mhz"]),
                "profile":       r["profile"],
                "stress_cores":  int(r["stress_cores"]),
                "elapsed_s":     int(r["elapsed_s"]),
                "temp_c":        int(r["temp_c"]),
                "fan1_rpm":      int(r["fan1_rpm"]),
                "fan2_rpm":      int(r["fan2_rpm"]),
                "throttle_ms":   int(r["throttle_total_ms"]),
                "cur_freq_mhz":  int(r["cur_freq_mhz"]),
            })
    return rows


PS = {"power-saver": "PS", "balanced": "bal", "performance": "perf"}
PC = {"performance": "tab:red", "balanced": "tab:blue", "power-saver": "tab:green"}


def load_label(c, nproc=16):
    if c == 0: return "idle"
    if c >= nproc: return f"all({c})"
    return f"{c}c"


# ═══════════════════════════════════════════════════════════════════
# 1. Temperature ramp-up over time (one subplot per cap, lines per profile)
# ═══════════════════════════════════════════════════════════════════
def plot_temp_ramp(rows, outdir):
    caps = sorted(set(r["cap_mhz"] for r in rows))
    profiles = sorted(set(r["profile"] for r in rows))

    fig, axes = plt.subplots(1, len(caps), figsize=(4*len(caps), 4),
                              squeeze=False, sharey=True)
    fig.suptitle("Temperature During Load Ramp-Up", fontsize=13, fontweight="bold")

    for ci, cap in enumerate(caps):
        ax = axes[0][ci]
        for p in profiles:
            pts = sorted([r for r in rows if r["cap_mhz"]==cap and r["profile"]==p],
                         key=lambda r: r["elapsed_s"])
            if not pts: continue
            ax.plot([r["elapsed_s"] for r in pts],
                    [r["temp_c"] for r in pts],
                    linewidth=1.2, label=PS.get(p, p), color=PC.get(p, "gray"))
        ax.set_title(f"{cap} MHz", fontsize=10)
        ax.set_xlabel("Time (s)", fontsize=8)
        if ci == 0: ax.set_ylabel("Temperature (°C)")
        ax.legend(fontsize=7)
        ax.grid(True, alpha=0.3)
        ax.set_ylim(35, 105)

    fig.tight_layout()
    fig.savefig(outdir / "01-temp-ramp.png", dpi=150)
    plt.close(fig)
    print(f"  01-temp-ramp.png")


# ═══════════════════════════════════════════════════════════════════
# 2. Fan RPM during ramp-up
# ═══════════════════════════════════════════════════════════════════
def plot_fan_ramp(rows, outdir):
    caps = sorted(set(r["cap_mhz"] for r in rows))
    profiles = sorted(set(r["profile"] for r in rows))

    fig, axes = plt.subplots(1, len(caps), figsize=(4*len(caps), 4),
                              squeeze=False, sharey=True)
    fig.suptitle("Fan RPM During Load Ramp-Up", fontsize=13, fontweight="bold")

    for ci, cap in enumerate(caps):
        ax = axes[0][ci]
        for p in profiles:
            pts = sorted([r for r in rows if r["cap_mhz"]==cap and r["profile"]==p],
                         key=lambda r: r["elapsed_s"])
            if not pts: continue
            fans = [max(r["fan1_rpm"], r["fan2_rpm"]) for r in pts]
            ax.plot([r["elapsed_s"] for r in pts], fans,
                    linewidth=1.2, label=PS.get(p, p), color=PC.get(p, "gray"))
        ax.set_title(f"{cap} MHz", fontsize=10)
        ax.set_xlabel("Time (s)", fontsize=8)
        if ci == 0: ax.set_ylabel("Fan RPM")
        ax.legend(fontsize=7)
        ax.grid(True, alpha=0.3)

    fig.tight_layout()
    fig.savefig(outdir / "02-fan-ramp.png", dpi=150)
    plt.close(fig)
    print(f"  02-fan-ramp.png")


# ═══════════════════════════════════════════════════════════════════
# 3. Fan RPM vs Temperature scatter
# ═══════════════════════════════════════════════════════════════════
def plot_fan_vs_temp(rows, outdir):
    profiles = sorted(set(r["profile"] for r in rows))

    fig, ax = plt.subplots(figsize=(8, 5))
    fig.suptitle("Fan RPM vs Temperature (all tests)", fontsize=13, fontweight="bold")

    for p in profiles:
        pts = [r for r in rows if r["profile"] == p]
        temps = [r["temp_c"] for r in pts]
        fans = [max(r["fan1_rpm"], r["fan2_rpm"]) for r in pts]
        ax.scatter(temps, fans, s=6, alpha=0.4, label=PS.get(p, p), color=PC.get(p, "gray"))

    ax.axhline(y=4000, color="orange", linestyle="--", alpha=0.7, label="quiet threshold (4k)")
    ax.set_xlabel("Temperature (°C)")
    ax.set_ylabel("Fan RPM (max of both)")
    ax.legend()
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(outdir / "03-fan-vs-temp.png", dpi=150)
    plt.close(fig)
    print(f"  03-fan-vs-temp.png")


# ═══════════════════════════════════════════════════════════════════
# 4. Step summaries: boundary heatmaps
# ═══════════════════════════════════════════════════════════════════
def compute_step_summary(rows):
    by_step = defaultdict(list)
    for r in rows:
        by_step[(r["cap_mhz"], r["profile"], r["stress_cores"])].append(r)

    results = []
    for (cap, prof, cores), samples in sorted(by_step.items()):
        tail = samples[-5:] if len(samples) >= 5 else samples
        temps = [s["temp_c"] for s in tail]
        fans = [max(s["fan1_rpm"], s["fan2_rpm"]) for s in tail]
        freqs = [s["cur_freq_mhz"] for s in tail]
        thr_start = samples[0]["throttle_ms"]
        thr_end = samples[-1]["throttle_ms"]
        results.append({
            "cap_mhz": cap, "profile": prof, "stress_cores": cores,
            "temp_avg": np.mean(temps), "temp_max": max(temps),
            "fan_avg": np.mean(fans), "fan_max": max(fans),
            "freq_avg": np.mean(freqs),
            "throttle_delta_ms": thr_end - thr_start,
        })
    return results


def plot_boundary_map(ss, outdir):
    profiles = sorted(set(s["profile"] for s in ss))
    caps = sorted(set(s["cap_mhz"] for s in ss))
    cores_list = sorted(set(s["stress_cores"] for s in ss))

    for prof in profiles:
        fig, (ax_t, ax_f, ax_th) = plt.subplots(
            1, 3, figsize=(15, max(3, len(cores_list)*0.7+1)))
        fig.suptitle(f"Boundary Map — {prof}", fontsize=13, fontweight="bold")

        for ax, metric, label, cmap, vmin, vmax in [
            (ax_t, "temp_avg", "Avg Temp (°C)", "coolwarm", 40, 100),
            (ax_f, "fan_avg", "Avg Fan (RPM)", "YlOrRd", 0, None),
            (ax_th, "throttle_delta_ms", "Throttle (ms)", "Reds", 0, None),
        ]:
            data = np.full((len(cores_list), len(caps)), np.nan)
            for s in ss:
                if s["profile"] != prof: continue
                ci = caps.index(s["cap_mhz"])
                li = cores_list.index(s["stress_cores"])
                data[li][ci] = s[metric]

            if vmax is None:
                vmax = np.nanmax(data) if not np.all(np.isnan(data)) else 1
                if vmax == 0: vmax = 1

            im = ax.imshow(data, aspect="auto", cmap=cmap, origin="lower",
                           vmin=vmin, vmax=vmax)
            ax.set_xticks(range(len(caps)))
            ax.set_xticklabels([str(c) for c in caps], fontsize=8)
            ax.set_yticks(range(len(cores_list)))
            ax.set_yticklabels([load_label(c) for c in cores_list], fontsize=8)
            ax.set_xlabel("Freq Cap (MHz)")
            ax.set_title(label, fontsize=10)

            for li in range(len(cores_list)):
                for ci in range(len(caps)):
                    v = data[li][ci]
                    if not np.isnan(v):
                        ax.text(ci, li, f"{v:.0f}", ha="center", va="center",
                                fontsize=7, color="white" if v > (vmin+vmax)/2 else "black")
            fig.colorbar(im, ax=ax, shrink=0.8)

        fig.tight_layout()
        fig.savefig(outdir / f"04-boundary-{PS.get(prof, prof)}.png", dpi=150)
        plt.close(fig)
        print(f"  04-boundary-{PS.get(prof, prof)}.png")


# ═══════════════════════════════════════════════════════════════════
# 5. Profile comparison: temp & fan vs load for each cap
# ═══════════════════════════════════════════════════════════════════
def plot_profile_comparison(ss, outdir):
    caps = sorted(set(s["cap_mhz"] for s in ss))
    profiles = sorted(set(s["profile"] for s in ss))

    fig, axes = plt.subplots(2, len(caps), figsize=(4*len(caps), 7), squeeze=False)
    fig.suptitle("Profile Comparison: Temp & Fan vs Load", fontsize=13, fontweight="bold")

    for ci, cap in enumerate(caps):
        ax_t, ax_f = axes[0][ci], axes[1][ci]
        for p in profiles:
            pts = sorted([s for s in ss if s["cap_mhz"]==cap and s["profile"]==p],
                         key=lambda s: s["stress_cores"])
            if not pts: continue
            xs = [s["stress_cores"] for s in pts]
            ax_t.plot(xs, [s["temp_avg"] for s in pts],
                      "o-", label=PS.get(p, p), color=PC.get(p, "gray"), lw=2)
            ax_f.plot(xs, [s["fan_avg"] for s in pts],
                      "o-", label=PS.get(p, p), color=PC.get(p, "gray"), lw=2)
        ax_t.set_title(f"{cap} MHz", fontsize=10)
        if ci == 0:
            ax_t.set_ylabel("Temp (°C)")
            ax_f.set_ylabel("Fan RPM")
        ax_t.legend(fontsize=7); ax_t.grid(True, alpha=0.3)
        ax_f.set_xlabel("Stress cores"); ax_f.legend(fontsize=7); ax_f.grid(True, alpha=0.3)
        ax_f.axhline(y=4000, color="orange", linestyle="--", alpha=0.5)

    fig.tight_layout()
    fig.savefig(outdir / "05-profile-comparison.png", dpi=150)
    plt.close(fig)
    print(f"  05-profile-comparison.png")


# ═══════════════════════════════════════════════════════════════════
# 6. Actual freq vs cap at max load
# ═══════════════════════════════════════════════════════════════════
def plot_actual_freq(ss, outdir):
    caps = sorted(set(s["cap_mhz"] for s in ss))
    profiles = sorted(set(s["profile"] for s in ss))
    max_cores = max(s["stress_cores"] for s in ss)

    fig, ax = plt.subplots(figsize=(8, 5))
    fig.suptitle(f"Actual Freq vs Cap (stress={load_label(max_cores)})",
                 fontsize=13, fontweight="bold")
    ax.plot([caps[0], caps[-1]], [caps[0], caps[-1]], "k--", alpha=0.3, label="ideal")

    for p in profiles:
        pts = sorted([s for s in ss if s["profile"]==p and s["stress_cores"]==max_cores],
                     key=lambda s: s["cap_mhz"])
        if not pts: continue
        ax.plot([s["cap_mhz"] for s in pts], [s["freq_avg"] for s in pts],
                "o-", label=PS.get(p, p), color=PC.get(p, "gray"), lw=2)

    ax.set_xlabel("Freq Cap (MHz)")
    ax.set_ylabel("Actual Freq (MHz)")
    ax.legend(); ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(outdir / "06-actual-freq.png", dpi=150)
    plt.close(fig)
    print(f"  06-actual-freq.png")


# ═══════════════════════════════════════════════════════════════════
# 7. Key boundaries summary
# ═══════════════════════════════════════════════════════════════════
def plot_summary(ss, outdir):
    profiles = sorted(set(s["profile"] for s in ss))
    caps = sorted(set(s["cap_mhz"] for s in ss))

    fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(12, 5))
    fig.suptitle("Key Hardware Boundaries", fontsize=13, fontweight="bold")

    # Fan onset: min temp where fan_avg >= 4000 per cap × profile
    ax1.set_title("Fan >=4k RPM: Temp at Onset", fontsize=10)
    for p in profiles:
        temps = []
        for cap in caps:
            pts = sorted([s for s in ss if s["profile"]==p and s["cap_mhz"]==cap],
                         key=lambda s: s["stress_cores"])
            loud = [s for s in pts if s["fan_avg"] >= 4000]
            temps.append(loud[0]["temp_avg"] if loud else np.nan)
        ax1.plot(caps, temps, "o-", label=PS.get(p, p), color=PC.get(p, "gray"), lw=2, ms=8)
    ax1.set_xlabel("Freq Cap (MHz)")
    ax1.set_ylabel("Temp at Fan >=4k RPM (°C)")
    ax1.legend(); ax1.grid(True, alpha=0.3)

    # Throttle at max load
    ax2.set_title("Throttle at Max Load per Cap", fontsize=10)
    max_cores = max(s["stress_cores"] for s in ss)
    w = 0.25
    for pi, p in enumerate(profiles):
        pts = sorted([s for s in ss if s["profile"]==p and s["stress_cores"]==max_cores],
                     key=lambda s: s["cap_mhz"])
        if not pts: continue
        xs = [caps.index(s["cap_mhz"]) + pi*w - w for s in pts]
        ax2.bar(xs, [s["throttle_delta_ms"] for s in pts],
                width=w, label=PS.get(p, p), color=PC.get(p, "gray"), alpha=0.8)
    ax2.set_xticks(range(len(caps)))
    ax2.set_xticklabels([str(c) for c in caps])
    ax2.set_xlabel("Freq Cap (MHz)")
    ax2.set_ylabel("Throttle (ms) in 20s")
    ax2.legend(); ax2.grid(True, alpha=0.3, axis="y")

    fig.tight_layout()
    fig.savefig(outdir / "07-boundaries.png", dpi=150)
    plt.close(fig)
    print(f"  07-boundaries.png")


# ═══════════════════════════════════════════════════════════════════

def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <hw-data-*.csv>")
        sys.exit(1)

    csv_path = Path(sys.argv[1])
    outdir = csv_path.parent / "hw-charts"
    outdir.mkdir(exist_ok=True)

    print(f"Loading {csv_path}...")
    rows = load_csv(csv_path)
    n_tests = len(set((r["cap_mhz"], r["profile"]) for r in rows))
    print(f"  {len(rows)} samples across {n_tests} test runs")

    ss = compute_step_summary(rows)
    print(f"  {len(ss)} step summaries\n")
    print(f"Generating charts in {outdir}/")

    plot_temp_ramp(rows, outdir)
    plot_fan_ramp(rows, outdir)
    plot_fan_vs_temp(rows, outdir)
    plot_boundary_map(ss, outdir)
    plot_profile_comparison(ss, outdir)
    plot_actual_freq(ss, outdir)
    plot_summary(ss, outdir)

    # Text summary
    print(f"\n{'='*65}")
    print("  TEXT SUMMARY")
    print(f"{'='*65}")
    print(f"{'Cap':>6} {'Prof':>6} {'Load':>6} {'Temp':>6} {'Fan':>6} {'Thr':>6} {'Freq':>6}")
    print(f"{'MHz':>6} {'':>6} {'cores':>6} {'°C':>6} {'RPM':>6} {'ms':>6} {'MHz':>6}")
    print("-" * 65)
    for s in sorted(ss, key=lambda x: (x["profile"], x["cap_mhz"], x["stress_cores"])):
        print(f"{s['cap_mhz']:>6} {PS.get(s['profile'], s['profile']):>6} "
              f"{s['stress_cores']:>6} {s['temp_avg']:>5.0f}  {s['fan_avg']:>5.0f}  "
              f"{s['throttle_delta_ms']:>5}  {s['freq_avg']:>5.0f}")

    print(f"\nDone! Charts: {outdir}/")


if __name__ == "__main__":
    main()
