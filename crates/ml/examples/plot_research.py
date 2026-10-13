"""Plot completed JFQuant native research; requires matplotlib, never trains."""
import argparse
import csv
from datetime import datetime, timedelta
import json
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import matplotlib.dates as mdates


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="completed research output directory")
    args = parser.parse_args()
    root = args.output
    with (root / "nav.csv").open() as handle:
        rows = list(csv.DictReader(handle))
    if not rows:
        raise ValueError("empty NAV series")
    summary = json.loads((root / "summary.json").read_text())
    if summary.get("schema_version") != 4 or summary.get("execution", {}).get("mode") != "raw_integer_shares":
        raise ValueError("unsupported_result_format: requires schema 4 and raw_integer_shares")
    dates = [datetime.fromisoformat(row["date"]) for row in rows]
    # Initial capital matters to the first observation's drawdown.
    dates.insert(0, dates[0] - timedelta(days=1))
    curves = {
        key: [1.0] + [float(row[key]) for row in rows]
        for key in ("nav_after_fees", "csi500_price_index_nav")
    }
    colors = ["#15803d", "#374151"]
    labels = ["Selected A-share fees", "CSI 500 price index"]
    plt.rcParams.update({"font.size": 10, "axes.spines.top": False, "axes.spines.right": False})
    fig, axes = plt.subplots(2, 1, figsize=(12, 7.8), sharex=True,
                             gridspec_kw={"height_ratios": [2, 1]})
    for (key, values), color, label in zip(curves.items(), colors, labels):
        axes[0].plot(dates, values, color=color, label=label,
                     linewidth=1.7 if key == "nav_after_fees" else 1.2)
        if key in ("nav_after_fees", "csi500_price_index_nav"):
            peak = 1.0
            drawdown = []
            for value in values:
                peak = max(peak, value)
                drawdown.append((value / peak - 1) * 100)
            axes[1].plot(dates, drawdown, color=color, label=label, linewidth=1.3)
    axes[0].set_yscale("log")
    axes[0].set_ylabel("Cumulative NAV (log scale)")
    axes[0].legend(loc="upper left", ncol=2, frameon=False)
    axes[1].set_ylabel("Drawdown (%)")
    axes[1].xaxis.set_major_locator(mdates.YearLocator(2))
    axes[1].xaxis.set_major_formatter(mdates.DateFormatter("%Y"))
    for ax in axes:
        ax.grid(alpha=0.2)
        ax.margins(x=0.01)
    p = summary["aggregate"]
    fig.suptitle(f"JFQuant native annual walk-forward | CSI 500 | {len(summary['execution']['research_config']['factor_ids'])} GTJA factors",
                 fontsize=15, fontweight="bold", y=0.97)
    fig.text(0.5, 0.915,
             f"After fees: annualized {p['annualized_return']:.2%} | benchmark {p['benchmark_annualized_return']:.2%}"
             f" | max drawdown {p['max_drawdown']:.2%}", ha="center", fontsize=11)
    fig.text(0.1, 0.025,
             "Latest A-share fee snapshot on all dates. Rolling windows and portfolio settings frozen in execution.json.\n"
             "Raw-price integer fills, persistent account and dated company actions; price-index benchmark.",
             fontsize=9, color="#475569")
    fig.subplots_adjust(top=0.87, bottom=0.13, left=0.1, right=0.97, hspace=0.15)
    fig.savefig(root / "nav.png", dpi=170, facecolor="white")
    fig.savefig(root / "nav.svg", facecolor="white")
    plt.close(fig)


if __name__ == "__main__":
    main()
