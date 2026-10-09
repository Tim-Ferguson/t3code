// Original pull-request identity helpers plus the exact legacy metadata update
// expression from Orchestrator.ts. No network or repository source mutation.
import { readFileSync, writeFileSync, renameSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import * as DateTime from "../../apps/server/node_modules/effect/dist/DateTime.js";
import {
  legacyThreadPullRequestKey,
  normalizeThreadPullRequestKey,
  threadPullRequestsOf,
  threadPullRequestKeysEqual,
} from "../../packages/shared/src/threadPullRequests.ts";
const source = readFileSync(
  new URL("../../apps/server/src/orchestration-v2/Orchestrator.ts", import.meta.url),
  "utf8",
);
function block(name) {
  const start = source.indexOf(`function ${name}(`),
    end = source.indexOf("\n}", start);
  if (start < 0 || end < 0) throw new Error(`Missing original ${name}`);
  return source.slice(start, end + 2);
}
const begin = source.indexOf("            ...(command.linkedPullRequest === undefined");
const end = source.indexOf("            // regenerateTitle:", begin);
if (begin < 0 || end < 0) throw new Error("Original legacy metadata update expression changed");
const update = new Function(
  "DateTime",
  "legacyThreadPullRequestKey",
  "threadPullRequestsOf",
  "threadPullRequestKeysEqual",
  stripTypeScriptTypes(
    block("withPullRequestWatch") +
      "\n" +
      block("legacyPullRequestLink") +
      "\nconst update = (thread,command,now)=>({...thread," +
      source.slice(begin, end) +
      "});",
    { mode: "strip" },
  ) + ";return update;",
)(DateTime, legacyThreadPullRequestKey, threadPullRequestsOf, threadPullRequestKeysEqual);
const rows = [];
const urls = [
  "https://github.com/Owner/Repo/pull/7",
  "https://git.github.example/Owner/Repo/pull/7/files",
  "https://unknown.example/Owner/Repo/pull/7",
  "https://git.example:3000/Owner/Repo/pulls/7",
  "https://git.example/Group/Sub/Repo/-/merge_requests/7",
  "https://bitbucket.org/Owner/Repo/pull-requests/7",
  "https://dev.azure.com/Org/Project/_git/Repo/pullrequest/7",
  "https://Org.visualstudio.com/DefaultCollection/Project/_git/Repo/pullrequest/7",
  "http://[::1]:3000/Owner/Repo/pulls/7",
  "https://github.com/Owner/Repo/pull/9007199254740992",
  "not a url",
  "mailto:person@example.com",
];
for (const url of urls)
  for (const repository of ["Owner/Repo", " Org/Project/_git/Repo ", " Ä/ΟΣ "])
    for (const number of [7, 8])
      for (const fallback of [null, " FALLBACK.EXAMPLE "]) {
        const input = { repository, number, url };
        rows.push({
          type: "legacy",
          input,
          fallback,
          result: legacyThreadPullRequestKey(input, fallback ?? undefined),
        });
      }
const keys = [
  { host: "GITHUB.COM", repository: " Owner/Repo ", number: 7 },
  { host: "ssh.dev.azure.com", repository: "v3/Org/Project/Repo", number: 7 },
  { host: "vs-ssh.visualstudio.com", repository: "v3/Org/Project/Repo", number: 7 },
  { host: "Org.visualstudio.com", repository: "DefaultCollection/Project/_git/Repo", number: 7 },
  {
    host: "git.example",
    repository: "owner/repo",
    number: 7,
    url: "https://git.example:3000/owner/repo/pulls/7",
  },
  {
    host: "git.example",
    repository: "owner/repo",
    number: 8,
    url: "https://git.example:3000/owner/repo/pulls/7",
  },
  {
    host: "git.example",
    repository: "different/repo",
    number: 7,
    url: "https://git.example:3000/owner/repo/pulls/7",
  },
  {
    host: "git.example",
    repository: "owner/repo",
    number: 7,
    authority: "OTHER.EXAMPLE:4000",
    url: "https://git.example:3000/owner/repo/pulls/7",
  },
  { host: "\u00a0Ä.EXAMPLE\ufeff", repository: " ΟΣ/İ ", number: 7 },
  { host: "", repository: "Owner/Repo", number: 7 },
];
for (const input of keys)
  for (const number of [input.number, 99]) {
    const key = { ...input, number };
    rows.push({ type: "normalize", input: key, result: normalizeThreadPullRequestKey(key) });
  }
const old = { projectId: "project", repository: "owner/repo", number: 7, url: urls[0] };
const other = {
  host: "github.com",
  repository: "other/repo",
  number: 99,
  url: "https://github.com/other/repo/pull/99",
  source: "stack",
  linkedAt: "2025-01-01T00:00:00.000Z",
  snapshot: null,
  stack: null,
};
const now = "2026-01-01T00:00:00.000Z";
for (const state of ["legacy", "many", "empty", "ports"])
  for (const linked of [
    null,
    old,
    { projectId: "project", repository: "Repo", number: 7, url: urls[6] },
  ])
    for (const withWatch of [false, true]) {
      const entry = {
        ...legacyThreadPullRequestKey(old),
        url: old.url,
        source: "manual",
        linkedAt: "2025-01-01T00:00:00.000Z",
        snapshot: null,
        stack: null,
        ...(withWatch ? { watch: { revision: 4, enabled: true } } : {}),
      };
      const thread = { linkedPullRequest: old };
      if (state === "many") thread.pullRequests = [entry, other];
      if (state === "empty") thread.pullRequests = [];
      if (state === "ports")
        thread.pullRequests = [
          entry,
          {
            host: "git.example",
            repository: "owner/repo",
            number: 7,
            url: urls[3],
            source: "manual",
            linkedAt: now,
            snapshot: null,
            stack: null,
          },
        ];
      const result = update(thread, { linkedPullRequest: linked }, DateTime.makeUnsafe(now));
      rows.push({
        type: "update",
        thread,
        linked,
        now,
        result: { linkedPullRequest: result.linkedPullRequest, pullRequests: result.pullRequests },
      });
    }
const target = new URL(
    "../crates/server/tests/fixtures/thread-pull-requests.jsonl",
    import.meta.url,
  ),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map(JSON.stringify).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
