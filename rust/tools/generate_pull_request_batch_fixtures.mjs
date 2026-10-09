// The aliased document the original reads several pull request summaries with, for
// crates/server/tests/pull_request_batch.rs to compare against. A null document means a lookup
// that cannot be written into a document, which is read on its own instead.
import { writeFileSync, renameSync } from "node:fs";
import * as J from "../../apps/server/src/pullRequest/gitHubPullRequestJson.ts";

const rows = [];
const cases = [
  [[{ repository: "acme/web", number: 7 }], false],
  [[{ repository: "acme/web", number: 7 }], true],
  [
    [
      { repository: "acme/web", number: 7 },
      { repository: "Acme.io/web-app_2", number: 12 },
      { repository: " acme/other ", number: 3 },
    ],
    true,
  ],
  [[], true],
  [[{ repository: "acme/web", number: 0 }], false],
  [[{ repository: "acme/web", number: -4 }], false],
  [[{ repository: "acme/web", number: 1.5 }], false],
  [[{ repository: "acme/web/extra", number: 1 }], false],
  [[{ repository: "acme", number: 1 }], false],
  [[{ repository: 'acme/we"b', number: 1 }], false],
  [
    [
      { repository: "acme/web", number: 1 },
      { repository: "bad repo/web", number: 2 },
    ],
    true,
  ],
  [[{ repository: "acme/web", number: 9007199254740991 }], false],
  [[{ repository: "acme/web", number: 9007199254740992 }], false],
];
for (const [requests, stacks] of cases)
  rows.push({ requests, stacks, query: J.buildPullRequestSummariesGraphQlQuery(requests, stacks) });
const target = new URL("../crates/server/tests/fixtures/pull-request-batch.jsonl", import.meta.url),
  temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(
  JSON.stringify({
    cases: rows.length,
    unbatchable: rows.filter((row) => row.query === null).length,
  }),
);
