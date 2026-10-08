# Backend benchmark, 2026-10-08

Completed 40 production desktop-backend launches at committed source `fcd48c83a`, using ten randomized paired fresh/reused rounds, seven disabled providers, isolated application state, and the same preserved native resource monitor. No renderer was launched. See [report.md](report.md) for results and limits.

`results.json` contains every raw case and request sample, both idle and post-work process snapshots, persisted-data fingerprints, and owned shutdown results. Diagnostic text is omitted from this committed copy; no credentials, SQLite databases, binaries, or generated runtime state are included. `summary.json`, `measurements.csv`, `metadata.json`, and `run.log` retain aggregates, UTC timestamps, host information, artifact hashes, and launch order. The source run directory in metadata is provenance, not a required input.

The harness snapshots use Node 24 built-in APIs. To repeat, update executable/bundle/monitor paths in `plan.json`, then run `node benchmark.mjs plan.json` from an idle machine. The original Node bundle uses `--import isolated-homedir.mjs` before loading application code. This directs Node homedir lookups to the scratch root while leaving HOME unchanged; both backends receive explicit T3/XDG/temp roots. Only captured child PIDs are shut down. The run creates a new scratch result directory and verifies matching ten-project/thirty-thread datasets before aggregating.

These measurements cover common backend endpoints, not full desktop/app parity or language overhead. Fresh means fresh application state, not cold OS caches. The original implements more services and some larger replies. p95 scope, response sizes, readiness polling precision, and sumRSS shared-page limits are documented in the report.

`original-dist-sha256.json` fingerprints the complete original production dist runtime and served asset tree (captured after timing, without a rebuild). The `.mjs.txt` copies preserve exact measured harness bytes; `.mjs` copies are runnable and may receive repository formatting. `metadata.json` explains sanitization and provenance.
