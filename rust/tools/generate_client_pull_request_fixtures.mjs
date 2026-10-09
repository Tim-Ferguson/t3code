// Runs the original's pure pull request rules (`classifyPullRequestChecks`,
// `describePullRequestChecks`, `groupPullRequestChecks`, `resolvePullRequestPrimaryControl`,
// `allowedPullRequestMergeMethods`, `resolveSelectedMergeMethod`, `resolveBaseFreshness`,
// `readableFailure`) and records what they answer. The Rust tests in t3-client replay the rows and
// require identical output. The functions are cut out of `pullRequestDetail.logic.ts` as they
// stand, because the module imports React-side code node cannot load.
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = new URL("../../", import.meta.url).pathname;
const source = readFileSync(
  join(root, "apps/web/src/components/pullRequest/pullRequestDetail.logic.ts"),
  "utf8",
);
const cut = (pattern) => {
  const match = source.match(pattern);
  if (!match) throw new Error(`not found: ${pattern}`);
  return match[0];
};
const fn = (name) => cut(new RegExp(`export function ${name}[\\s\\S]*?\\n}\\n`));
const pieces = [
  "type PullRequestMergeMethod = string; type PullRequestState = string; type PullRequestMergeability = string; type PullRequestBaseComparison = string; type PullRequestCheck = any; type PullRequestAction = string; type PullRequestActionableDetail = any; type ContractPullRequestChecksState = string; type PullRequestDetail = any;",
  cut(/export type PullRequestPrimaryControl =[\s\S]*?;\n/),
  cut(/export type PullRequestChecksState =[^;]*;\n/),
  fn("allowedPullRequestMergeMethods"),
  fn("resolveSelectedMergeMethod"),
  fn("resolvePullRequestPrimaryControl"),
  fn("isPullRequestConflicting"),
  fn("classifyPullRequestChecks"),
  fn("describePullRequestChecks"),
  fn("groupPullRequestChecks"),
  fn("resolveBaseFreshness"),
  cut(/const OPERATION_PREFIX = [^\n]*\n/),
  cut(/const TOOL_NOISE = \[[\s\S]*?\];\n/),
  cut(/const FAILURE_DETAIL_MAX_LENGTH = [^\n]*\n/),
  fn("readableFailure"),
  "function isPullRequestOperationError(){ return false }",
];
const directory = mkdtempSync(join(tmpdir(), "pull-request-"));
writeFileSync(join(directory, "logic.ts"), pieces.join("\n"));
const logic = await import(pathToFileURL(join(directory, "logic.ts")));

const rows = [];
const statuses = [
  "success",
  "failure",
  "pending",
  "cancelled",
  "action-required",
  "skipped",
  "neutral",
];
const check = (status) => ({ name: status, status, description: null, url: null });
const sets = [
  [],
  ...statuses.map((s) => [s]),
  ["success", "success"],
  ["success", "pending", "pending"],
  ["pending", "failure", "action-required", "success"],
  ["failure", "cancelled"],
  ["action-required", "action-required", "success"],
  ["skipped", "neutral", "success"],
  ["pending", "pending", "pending", "failure"],
];
for (const set of sets) {
  const checks = set.map(check);
  const groups = logic.groupPullRequestChecks(checks);
  rows.push({
    type: "checks",
    statuses: set,
    state: logic.classifyPullRequestChecks(checks),
    text: logic.describePullRequestChecks(checks),
    groups: [groups.attention.length, groups.running.length, groups.completed.length],
  });
}
for (const state of ["open", "merged", "closed"])
  for (const isDraft of [false, true])
    for (const mergeability of ["mergeable", "conflicting", "unknown"])
      for (const checksState of [null, "passing", "pending", "failing", "none"])
        for (const autoMergeEnabled of [undefined, false, true])
          for (const hasMergeMethod of [false, true])
            for (const [canMerge, canMarkReady, canEnableAutoMerge] of [
              [true, true, true],
              [false, false, false],
              [true, false, true],
              [false, true, false],
            ])
              rows.push({
                type: "primary",
                input: {
                  state,
                  isDraft,
                  mergeability,
                  checksState,
                  autoMergeEnabled: autoMergeEnabled ?? null,
                  hasMergeMethod,
                  canMerge,
                  canMarkReady,
                  canEnableAutoMerge,
                },
                answer: logic.resolvePullRequestPrimaryControl({
                  state,
                  isDraft,
                  mergeability,
                  checksState,
                  autoMergeEnabled,
                  hasMergeMethod,
                  canMerge,
                  canMarkReady,
                  canEnableAutoMerge,
                }),
              });
for (const methods of [[], ["merge"], ["merge", "squash", "rebase"], ["squash", "rebase"]])
  for (const allowed of [
    { merge: true, squash: true, rebase: true },
    { merge: false, squash: true, rebase: false },
    { merge: true, squash: false, rebase: true },
    { merge: false, squash: false, rebase: false },
  ]) {
    const detail = { capabilities: { mergeMethods: methods }, mergeCapabilities: allowed };
    const list = logic.allowedPullRequestMergeMethods(detail);
    rows.push({
      type: "methods",
      methods,
      allowed,
      list,
      picks: ["merge", "squash", "rebase"].map((preferred) =>
        logic.resolveSelectedMergeMethod(list, preferred),
      ),
    });
  }
for (const state of ["open", "closed"])
  for (const mergeability of ["mergeable", "conflicting", "unknown"])
    for (const baseComparison of [undefined, "behind", "identical"])
      for (const behindBy of [undefined, 5])
        for (const offered of [undefined, ["merge", "rebase"]])
          for (const allowed of [undefined, ["rebase"], ["merge", "rebase"]]) {
            const input = {
              state,
              mergeability,
              ...(baseComparison ? { baseComparison } : {}),
              ...(behindBy !== undefined ? { behindBy } : {}),
              capabilities: { ...(offered ? { updateMethods: offered } : {}) },
              viewerPermissions: { ...(allowed ? { updateMethods: allowed } : {}) },
            };
            rows.push({
              type: "freshness",
              input,
              answer: logic.resolveBaseFreshness(input) ?? null,
            });
          }
for (const message of [
  "",
  "Pull request operation merge failed: Base branch was modified.",
  "gh failed.",
  "exited with code 1",
  "unknown error",
  "x".repeat(400),
  "The host said no.",
])
  rows.push({ type: "failure", message, answer: logic.readableFailure(message, "HINT") });

writeFileSync(
  new URL("../crates/client/tests/fixtures/pull_request_logic.json", import.meta.url),
  JSON.stringify(rows),
);
console.log(`${rows.length} rows`);
