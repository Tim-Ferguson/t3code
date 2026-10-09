// Development oracle: runs the original `parseGitCloneProgressLine` over git's clone progress
// output and records each line's parse. The Rust parser replays every row (project clone
// progress, `source_control_repository.rs`). Run: node rust/tools/generate_project_clone_fixtures.mjs
import { renameSync, writeFileSync } from "node:fs";
import { parseGitCloneProgressLine } from "../../apps/server/src/project/gitCloneProgress.ts";

const lines = [
  "Cloning into 'repo'...",
  "remote: Enumerating objects: 1234, done.",
  "remote: Counting objects:  45% (45/100)",
  "remote: Counting objects: 100% (100/100), done.",
  "remote: Compressing objects:  12% (3/25)",
  "remote: Compressing objects: 100% (25/25), done.",
  "Receiving objects:   0% (1/2000)",
  "Receiving objects:  45% (900/2000), 12.30 MiB | 5.00 MiB/s",
  "Receiving objects: 100% (2000/2000), 40.00 MiB | 9.10 MiB/s, done.",
  "Receiving objects: 100% (2000/2000), done.",
  "Resolving deltas:  50% (10/20)",
  "Resolving deltas: 100% (20/20), done.",
  "Updating files:  99% (990/1000)",
  "Updating files: 100% (1000/1000), done.",
  "Checking out files:  10% (1/10)",
  "  Receiving objects:  7% (7/100)  ",
  "Receiving objects: 150% (3/2)",
  "Receiving objects: 100% (1/1), 1.00 KiB | 1.00 KiB/s, done.",
  "remote: Total 5 (delta 0), reused 0 (delta 0), pack-reused 0",
  "fatal: repository 'x' not found",
  "warning: You appear to have cloned an empty repository.",
  "hint: something",
  "",
  "Receiving objects",
  "Receiving objects: abc",
];
const rows = lines.map((line) => ({ line, parsed: parseGitCloneProgressLine(line) }));
const out = new URL("../crates/server/tests/fixtures/project-clone-progress.json", import.meta.url)
  .pathname;
writeFileSync(out + ".tmp", JSON.stringify(rows, null, 1) + "\n");
renameSync(out + ".tmp", out);
console.log(`wrote ${rows.length} rows to ${out}`);
