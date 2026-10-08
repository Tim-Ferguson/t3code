# Production backend benchmark

UTC: 2026-10-08T20:58:35.469Z → 2026-10-08T21:00:52.320Z

Source: committed fcd48c83a; 40 launches, 0 errors.

Both use isolated application state, all seven providers disabled, and the same preserved native resource monitor. Fresh/reused describes application state, not cold OS caches. No renderer is included.

The current Rust port implements fewer services. Config replies were about 23 KB versus 34 KB and seeded shell replies about 32 KB versus 44 KB; these endpoints have different payloads. The thread projection (1,065 bytes) and no-op probe (70 bytes) replies match in size. The ratios below describe this measured subset, not language overhead or completed full-app parity.

Normalized seeded dataset SHA256: `bc53251c2d3850f518c53e1572ea2718d973fdc41c76306158fef6f99a96f380`. Each successful case verifies ten projects and thirty threads.

Startup/RSS/shutdown percentiles cover ten launches per backend and state phase. Read percentiles pool 1,000 sequential warmed requests per backend/phase; project/thread writes pool fresh-state operations. p95 uses the nearest-rank method (with ten startup samples, p95 is the maximum). Raw samples and per-round medians are retained.

## fresh state

| Metric                                 | Unit |   Original median / p95 |     Rust median / p95 | Original ÷ Rust median |
| -------------------------------------- | ---- | ----------------------: | --------------------: | ---------------------: |
| HTTP readiness                         | ms   |     1883.368 / 2115.252 |      39.358 / 132.095 |                 47.853 |
| Authenticated accepted project command | ms   |     1978.115 / 2230.665 |      48.931 / 152.861 |                 40.427 |
| First authenticated config reply       | ms   |     1968.688 / 2217.785 |      47.498 / 148.064 |                 41.448 |
| Authenticated shell readiness          | ms   |     1982.154 / 2235.993 |      50.251 / 154.478 |                 39.445 |
| Idle backend root RSS                  | KiB  | 412088.000 / 415424.000 | 33360.000 / 33456.000 |                 12.353 |
| Idle backend + descendants sumRSS      | KiB  | 419920.000 / 423424.000 | 41296.000 / 41408.000 |                 10.169 |
| Post-work backend root RSS             | KiB  | 439928.000 / 442000.000 | 34536.000 / 34656.000 |                 12.738 |
| Post-work backend + descendants sumRSS | KiB  | 447744.000 / 450032.000 | 42456.000 / 42640.000 |                 10.546 |
| Seeded HTTP shell read                 | ms   |           5.110 / 9.096 |         1.399 / 3.305 |                  3.652 |
| Seeded thread projection RPC           | ms   |           1.638 / 4.487 |         0.202 / 0.609 |                  8.123 |
| Config RPC                             | ms   |          4.163 / 12.321 |         0.540 / 1.208 |                  7.706 |
| No-op probe RPC                        | ms   |           0.218 / 0.697 |         0.179 / 0.689 |                  1.215 |
| Persisted project create               | ms   |          4.225 / 10.692 |         0.464 / 2.835 |                  9.111 |
| Persisted thread create                | ms   |          3.532 / 12.909 |         0.460 / 2.098 |                  7.681 |
| Owned backend shutdown                 | ms   |         44.387 / 76.619 |        5.246 / 38.489 |                  8.461 |

Response sizes (median serialized bytes):

| Reply                 |  Original |      Rust |
| --------------------- | --------: | --------: |
| First config          | 33635.000 | 23363.000 |
| Config RPC            | 33636.000 | 23364.000 |
| Seeded HTTP shell     | 43716.000 | 31886.000 |
| Thread projection RPC |  1065.000 |  1065.000 |
| No-op probe RPC       |    70.000 |    70.000 |

## reused state

| Metric                                 | Unit |   Original median / p95 |     Rust median / p95 | Original ÷ Rust median |
| -------------------------------------- | ---- | ----------------------: | --------------------: | ---------------------: |
| HTTP readiness                         | ms   |     1869.891 / 3999.837 |       24.422 / 86.069 |                 76.566 |
| Authenticated accepted project command | ms   |     1964.927 / 4623.051 |      33.509 / 109.295 |                 58.638 |
| First authenticated config reply       | ms   |     1958.696 / 4591.369 |      32.472 / 104.234 |                 60.320 |
| Authenticated shell readiness          | ms   |     1973.786 / 4652.470 |      35.637 / 114.241 |                 55.385 |
| Idle backend root RSS                  | KiB  | 402200.000 / 406192.000 | 34152.000 / 34352.000 |                 11.777 |
| Idle backend + descendants sumRSS      | KiB  | 410176.000 / 414144.000 | 42104.000 / 42336.000 |                  9.742 |
| Post-work backend root RSS             | KiB  | 445328.000 / 456192.000 | 34360.000 / 34624.000 |                 12.961 |
| Post-work backend + descendants sumRSS | KiB  | 453288.000 / 464176.000 | 42264.000 / 42608.000 |                 10.725 |
| Seeded HTTP shell read                 | ms   |           5.287 / 7.414 |         1.317 / 3.381 |                  4.016 |
| Seeded thread projection RPC           | ms   |           1.640 / 3.254 |         0.199 / 0.514 |                  8.230 |
| Config RPC                             | ms   |           4.630 / 7.598 |         0.533 / 1.554 |                  8.687 |
| No-op probe RPC                        | ms   |           0.224 / 0.394 |         0.178 / 0.587 |                  1.258 |
| Owned backend shutdown                 | ms   |         43.092 / 53.068 |        4.011 / 15.839 |                 10.745 |

Response sizes (median serialized bytes):

| Reply                 |  Original |      Rust |
| --------------------- | --------: | --------: |
| First config          | 33634.000 | 23362.000 |
| Config RPC            | 33635.000 | 23363.000 |
| Seeded HTTP shell     | 43716.000 | 31886.000 |
| Thread projection RPC |  1065.000 |  1065.000 |
| No-op probe RPC       |    70.000 |    70.000 |

## Interpretation

Readiness uses HTTP probes approximately every 5 ms plus request/scheduling latency. Original Node startup includes a small pre-import homedir isolation shim. RSS is the backend process plus observed descendants; sumRSS can double-count shared pages and excludes desktop renderer processes outside the backend ancestry. Root and child process snapshots are retained before and after the request workload.

The original has more complete runtime/service coverage than the current Rust port. These results compare the measured common endpoints and datasets; they do not isolate language cost or establish equivalent full application performance. Config and other reply sizes are reported because payload and service coverage differ.

Shutdown checks: 0 cases with observed surviving descendants; 0 forced shutdowns.

Artifacts: results.json contains all forty raw case records (diagnostic text omitted); metadata.json (UTC/host/SHA256), plan.json, results.json, summary.json, measurements.csv, and harness/preload/report snapshots.

The complete original production dist file manifest is in `original-dist-sha256.json`, captured after timing with no original rebuild; file mtimes are retained. Exact measured harness bytes are preserved in `.mjs.txt` snapshots so repository formatting cannot alter them.
