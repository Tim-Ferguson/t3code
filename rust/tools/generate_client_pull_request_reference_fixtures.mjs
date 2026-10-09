// Runs the original `parsePullRequestReference` on what a user might paste to name a pull request
// and records its answer. The module only declares patterns and pure functions, so node runs it as it
// stands. The Rust test in t3-client replays the rows.
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = new URL("../../", import.meta.url).pathname;
const { parsePullRequestReference } = await import(
  pathToFileURL(join(root, "apps/web/src/pullRequestReference.ts"))
);
const inputs = [
  "",
  "   ",
  "7",
  "#7",
  " #42 ",
  "##7",
  "7a",
  "-3",
  "https://github.com/acme/web/pull/12",
  "https://github.com/acme/web/pull/12/files",
  "https://github.com/acme/web/pull/12?diff=split#x",
  "http://github.com/acme/web/pull/12",
  "https://github.com/acme/web/pulls/12",
  "https://github.com/acme/pull/12",
  "https://gitlab.com/acme/web/-/merge_requests/9",
  "https://gitlab.example.com/group/sub/web/-/merge_requests/9/diffs",
  "https://example.com/acme/web/-/merge_requests/9",
  "https://dev.azure.com/org/project/_git/repo/pullrequest/5",
  "https://org.visualstudio.com/project/_git/repo/pullrequest/5?_a=files",
  "https://codeberg.org/acme/web/pulls/3",
  "https://git.example.com/a/b/c/pulls/3",
  "http://forgejo.local/acme/web/pulls/3/files",
  "gh pr checkout 12",
  "gh pr checkout https://github.com/acme/web/pull/12",
  "GH PR CHECKOUT #12",
  "gh pr checkout",
  "gh pr checkout  ",
  "glab mr checkout 9",
  "glab mr checkout https://gitlab.com/acme/web/-/merge_requests/9",
  "tea pr checkout 4",
  "tea pulls checkout https://codeberg.org/acme/web/pulls/4",
  "az repos pr checkout --id 5",
  "az repos pr checkout -i 5",
  "az repos pr checkout --id=5",
  "az repos pr checkout --id=",
  "az repos pr checkout 5",
  "az repos pr checkout --detect true 5",
  "az repos pr checkout --id",
  "hello world",
  "feature-branch",
];
const rows = inputs.map((input) => ({ input, reference: parsePullRequestReference(input) }));
writeFileSync(
  join(root, "rust/crates/client/tests/fixtures/pull_request_reference.json"),
  `${JSON.stringify(rows, null, 1)}\n`,
);
console.log(`${rows.length} rows`);
