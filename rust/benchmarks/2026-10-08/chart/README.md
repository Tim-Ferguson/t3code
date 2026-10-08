# Shareable benchmark comparison

[PNG](comparison.png) · [SVG](comparison.svg) · [Exact plotted values](metrics.json)

The 1200 × 1400 chart compares measured common workloads between the original production build and the **incomplete Rust port**. It does not claim feature equivalence, isolate language overhead, or establish full application performance parity.

The chart uses the warm-profile desktop visibility medians from [desktop/comparison.json](../desktop/comparison.json) and fresh-state backend measurements from [backend/summary.json](../backend/summary.json). Desktop visibility is a disconnected, UI-only native lifecycle marker, not first paint or usable UI. The two runtimes have different feature coverage and startup work. Desktop fresh-profile results remain in the full report and are not combined with the warm-profile headline.

Startup and memory values summarize ten rounds per runtime and condition. RPC values are pooled medians of 1,000 sequential warmed requests; thread creation values pool 300 persisted writes. Memory is the sum of observed backend/descendant RSS, including the same native monitor, and may double-count shared pages. Providers are disabled. Fresh application state is not a cold OS-cache measurement.

Every panel uses its own labeled, linear scale from zero. Bars have exact proportional lengths, including the short Rust accepted-command bar; there is no minimum bar width. Speed ratios use unrounded medians. The memory headline rounds the measured 90.1657% reduction to 90%.

## Regenerate

From the repository root, using Python 3 and the installed macOS Swift/AppKit SDK:

```sh
python3 rust/benchmarks/2026-10-08/chart/generate.py
```

The generator reads the retained summaries, checks round counts and equal RPC response lengths, writes SVG/scene/metric files, and uses the small `render.swift` scene renderer for PNG export. The SVG and PNG share the same layout and measurements. Swift's temporary module cache is removed on completion. No Python packages, browser, network access, or dependency installation are required.

To regenerate just the portable vector/metric artifacts with Python's standard library:

```sh
python3 rust/benchmarks/2026-10-08/chart/generate.py --svg-only
```

The PNG uses installed Helvetica Neue; the SVG includes Helvetica/Arial fallbacks. `metrics.json` records the two input summary SHA-256 values. The full backend and desktop reports retain methodology and raw sample evidence.
