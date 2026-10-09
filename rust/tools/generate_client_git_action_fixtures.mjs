// Runs the original's pure git action rules (`buildMenuItems`, `resolveQuickAction`,
// `requiresDefaultBranchConfirmation`, `resolveDefaultBranchActionDialogCopy`,
// `resolveThreadBranchUpdate`, `buildGitActionProgressStages`) over a matrix of statuses and records
// what they answer. The Rust tests in t3-client replay the rows and require identical output.
// `GitActionsControl.logic.ts` imports a React module node cannot load, so that one import is
// pointed at the shared terminology module it re-exports.
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = new URL("../../", import.meta.url).pathname;
const logicPath = join(root, "apps/web/src/components/GitActionsControl.logic.ts");
const source = readFileSync(logicPath, "utf8")
  .replace(
    '"../sourceControlPresentation"',
    JSON.stringify(join(root, "packages/shared/src/sourceControl.ts")),
  )
  .replaceAll('"@t3tools/shared/git"', JSON.stringify(join(root, "packages/shared/src/git.ts")));
const directory = mkdtempSync(join(tmpdir(), "git-actions-"));
writeFileSync(join(directory, "logic.ts"), source);
const logic = await import(pathToFileURL(join(directory, "logic.ts")));

const rows = [];
const statuses = [];
for (const refName of [null, "feature/x", "main"])
  for (const hasWorkingTreeChanges of [false, true])
    for (const hasUpstream of [false, true])
      for (const aheadCount of [0, 2])
        for (const behindCount of [0, 1])
          for (const aheadOfDefaultCount of [undefined, 0, 3])
            for (const pr of [null, "open", "merged"])
              for (const isDefaultRef of [false, true])
                for (const hasPrimaryRemote of [false, true])
                  statuses.push({
                    refName,
                    hasWorkingTreeChanges,
                    hasUpstream,
                    aheadCount,
                    behindCount,
                    aheadOfDefaultCount,
                    pr,
                    isDefaultRef,
                    hasPrimaryRemote,
                  });
for (const kind of ["gitlab", "azure-devops"])
  for (const hasWorkingTreeChanges of [false, true])
    statuses.push({
      refName: "f",
      hasWorkingTreeChanges,
      hasUpstream: true,
      aheadCount: 1,
      behindCount: 0,
      aheadOfDefaultCount: 1,
      pr: null,
      isDefaultRef: false,
      hasPrimaryRemote: true,
      kind,
    });

const toWire = (s) => ({
  refName: s.refName,
  hasWorkingTreeChanges: s.hasWorkingTreeChanges,
  hasUpstream: s.hasUpstream,
  aheadCount: s.aheadCount,
  behindCount: s.behindCount,
  ...(s.aheadOfDefaultCount === undefined ? {} : { aheadOfDefaultCount: s.aheadOfDefaultCount }),
  pr: s.pr === null ? null : { state: s.pr },
  ...(s.kind ? { sourceControlProvider: { kind: s.kind, name: s.kind, baseUrl: "" } } : {}),
});
const slim = (item) => ({ ...item, icon: undefined, kind: undefined });
for (const status of statuses) {
  const wire = toWire(status);
  const quick = logic.resolveQuickAction(wire, false, status.isDefaultRef, status.hasPrimaryRemote);
  const menu = logic
    .buildMenuItems(wire, false, status.hasPrimaryRemote)
    .map((item) => [item.id, item.label, item.disabled]);
  rows.push({
    type: "status",
    status,
    quick: {
      label: quick.label,
      disabled: quick.disabled,
      kind: quick.kind,
      action: quick.action ?? null,
      hint: quick.hint ?? null,
    },
    menu,
  });
}
rows.push({
  type: "busy",
  quick: logic.resolveQuickAction(toWire(statuses[0]), true, false, true),
  menu: logic
    .buildMenuItems(toWire(statuses[1500]), true, true)
    .map((item) => [item.id, item.label, item.disabled]),
});
rows.push({
  type: "absent",
  quick: logic.resolveQuickAction(null, false),
  menu: logic.buildMenuItems(null, false),
});

for (const action of ["commit", "push", "create_pr", "commit_push", "commit_push_pr"])
  for (const isDefault of [false, true])
    rows.push({
      type: "confirm",
      action,
      isDefault,
      answer: logic.requiresDefaultBranchConfirmation(action, isDefault),
    });
const copyTerms = [undefined, { shortLabel: "MR", singular: "merge request" }];
for (const action of ["push", "create_pr", "commit_push", "commit_push_pr"])
  for (const includesCommit of [false, true])
    for (const terminology of copyTerms)
      rows.push({
        type: "copy",
        action,
        includesCommit,
        terminology: terminology ? "gitlab" : null,
        copy: logic.resolveDefaultBranchActionDialogCopy({
          action,
          branchName: "main",
          includesCommit,
          ...(terminology ? { terminology } : {}),
        }),
      });
for (const result of [
  { branch: { status: "created", name: "t3code/new" } },
  { branch: { status: "created" } },
  { branch: { status: "skipped_not_requested", name: "x" } },
])
  rows.push({ type: "branch", result, update: logic.resolveThreadBranchUpdate(result) });
for (const action of ["commit", "push", "create_pr", "commit_push", "commit_push_pr"])
  for (const hasCustomCommitMessage of [false, true])
    for (const featureBranch of [false, true])
      rows.push({
        type: "stages",
        action,
        hasCustomCommitMessage,
        featureBranch,
        stages: logic.buildGitActionProgressStages({
          action,
          hasCustomCommitMessage,
          hasWorkingTreeChanges: true,
          featureBranch,
          pushTarget: "origin/main",
          shouldPushBeforePr: true,
        }),
      });

writeFileSync(
  new URL("../crates/client/tests/fixtures/git_actions.json", import.meta.url),
  JSON.stringify(rows),
);
console.log(`${rows.length} rows`);
