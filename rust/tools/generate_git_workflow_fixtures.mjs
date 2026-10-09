// Runs the pure helpers of GitManager.ts that decide which pull request a branch shows, for
// crates/server/src/git_workflow_status.rs to replay.
import { writeFileSync, renameSync } from "node:fs";
import * as Manager from "../../apps/server/src/git/GitManager.ts";
import * as Duration from "../../apps/server/node_modules/effect/dist/Duration.js";
const rows = [];
for (const failures of [0, 1, 2, 3, 4, 5, 8, 12, 40])
  rows.push({
    type: "failureTtl",
    failures,
    millis: Duration.toMillis(Manager.prLookupFailureTtl(failures)),
  });
for (const url of [
  null,
  "",
  "git@github.com:acme/web.git",
  "https://github.com/acme/web",
  "https://github.com/acme/web.git/",
  "ssh://git@github.com/acme/web.git",
  "https://gitlab.example.com/group/sub/repo.git",
  "git://host/a/b",
  "not a url",
  "https://github.com/solo",
  "https://dev.azure.com/org/proj/_git/repo",
])
  rows.push({
    type: "repository",
    url,
    result: Manager.parseRepositoryNameWithOwnerFromRemoteUrl(url),
  });
const pr = (over) => ({
  number: 1,
  title: "T",
  url: "https://github.com/acme/web/pull/1",
  baseRefName: "main",
  headRefName: "feature",
  state: "open",
  updatedAt: { _tag: "None" },
  ...over,
});
const heads = [
  {
    headBranch: "feature",
    headRepositoryNameWithOwner: null,
    headRepositoryOwnerLogin: null,
    isCrossRepository: false,
  },
  {
    headBranch: "feature",
    headRepositoryNameWithOwner: "fork/web",
    headRepositoryOwnerLogin: "fork",
    isCrossRepository: true,
  },
  {
    headBranch: "feature",
    headRepositoryNameWithOwner: "acme/web",
    headRepositoryOwnerLogin: "acme",
    isCrossRepository: false,
  },
  {
    headBranch: "feature",
    headRepositoryNameWithOwner: "Fork/Web",
    headRepositoryOwnerLogin: null,
    isCrossRepository: true,
  },
  {
    headBranch: "other",
    headRepositoryNameWithOwner: null,
    headRepositoryOwnerLogin: null,
    isCrossRepository: false,
  },
  {
    headBranch: "feature",
    headRepositoryNameWithOwner: null,
    headRepositoryOwnerLogin: "fork",
    isCrossRepository: false,
  },
];
const prs = [
  pr({}),
  pr({ isCrossRepository: false }),
  pr({ isCrossRepository: true }),
  pr({ isCrossRepository: true, headRepositoryOwnerLogin: "fork" }),
  pr({ isCrossRepository: true, headRepositoryNameWithOwner: "fork/web" }),
  pr({ isCrossRepository: true, headRepositoryOwnerLogin: "other" }),
  pr({
    isCrossRepository: false,
    headRepositoryNameWithOwner: "acme/web",
    headRepositoryOwnerLogin: "acme",
  }),
  pr({ headRefName: "other" }),
  pr({
    isCrossRepository: true,
    headRepositoryOwnerLogin: "Fork",
    url: "https://github.com/acme/web/pull/1",
  }),
  pr({ headRepositoryNameWithOwner: "  " }),
];
for (const head of heads)
  for (const info of prs)
    rows.push({
      type: "matches",
      head,
      pr: {
        number: info.number,
        url: info.url,
        head: info.headRefName,
        isCrossRepository: info.isCrossRepository,
        headRepository: info.headRepositoryNameWithOwner ?? null,
        headOwner: info.headRepositoryOwnerLogin ?? null,
      },
      result: Manager.matchesBranchHeadContext(info, head),
    });
const target = new URL(
    "../crates/server/tests/fixtures/git-workflow-status.jsonl",
    import.meta.url,
  ),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map(JSON.stringify).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
