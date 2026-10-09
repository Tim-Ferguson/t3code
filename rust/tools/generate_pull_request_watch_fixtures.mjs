// Runs the original's pull request watch decisions (`evaluatePullRequestWatch` and
// `pullRequestWatchMessage`, apps/server/src/orchestration-v2/pullRequestWatch.ts) over pairs of
// (what the watch last told the agent, what the host shows now) and records what each means: what
// is new, the watch to record, whether this spends the last wake, and the wake the agent reads.
// `tests/fixtures/pull-request-watch.jsonl` is the witness `pull_request_watch.rs` is checked against.
import { writeFileSync } from "node:fs";
import {
  PULL_REQUEST_WATCH_WAKE_LIMIT,
  evaluatePullRequestWatch,
  pullRequestWatchMessage,
} from "../../apps/server/src/orchestration-v2/pullRequestWatch.ts";

// A fixed generator so the fixture is stable.
let seed = 20261009;
const random = () => {
  seed = (seed * 1664525 + 1013904223) % 4294967296;
  return seed / 4294967296;
};
const pick = (items) => items[Math.floor(random() * items.length)];
const chance = (p) => random() < p;

const STARTED = "2026-10-02T12:00:00.000Z";
const times = [
  "2026-10-02T11:59:59.000Z",
  "2026-10-02T12:00:00.000Z",
  "2026-10-02T12:00:01.000Z",
  "2026-10-02T12:05:00.000Z",
  "2026-10-02T12:05:00.000Z",
  "2026-10-02T13:00:00.000Z",
];
const names = ["lint", "test", "build", "CodeRabbit", "deploy"];
const statuses = [
  "pending",
  "action-required",
  "success",
  "failure",
  "skipped",
  "neutral",
  "cancelled",
];
const logins = ["agent-user", "Agent-User", "reviewer", "bot[bot]", "someone"];
const heads = [null, "aaaaaaaaaa", "bbbbbbbbbb", "ccccccc"];

const check = (name, status, required) => ({
  name,
  status,
  description: null,
  url: chance(0.7) ? `https://ci.example/${name}` : null,
  ...(required === undefined ? {} : { required }),
});
const remark = (id) => {
  const login = chance(0.1) ? null : pick(logins);
  const createdAt = pick(times);
  return {
    id,
    kind: pick(["comment", "review", "review-comment"]),
    author: login === null ? null : { login, name: null, avatarUrl: null },
    body: pick([
      "Please rename this.",
      "  spaced   out\n\ttext ",
      "<!-- hidden --> visible after",
      "",
      "LGTM",
      "LGTM",
      "Please rename this.",
    ]),
    createdAt,
    ...(chance(0.3) ? { editedAt: pick([null, ...times]) } : {}),
    url: chance(0.6) ? `https://github.com/o/r/pull/1#${id}` : null,
    path: chance(0.4) ? "src/index.ts" : null,
    reviewState: chance(0.3) ? pick(["APPROVED", "CHANGES_REQUESTED", "COMMENTED"]) : null,
  };
};
const randomWatch = () => {
  const told = chance(0.5) ? pick(names.slice(0, 3)) : null;
  const passed = chance(0.3);
  return {
    startedAt: STARTED,
    headSha: pick(heads),
    failedChecks: chance(0.4) ? [pick(names), ...(chance(0.3) ? [pick(names)] : [])] : [],
    passed,
    passedChecks: passed && chance(0.7) ? names.slice(0, 1 + Math.floor(random() * 3)) : [],
    remarksThrough: pick(times),
    remarkIds: chance(0.5) ? [`r${Math.floor(random() * 4)}`] : [],
    conflicting: chance(0.2),
    wakes: pick([0, 0, 1, 5, 8, 9, 9, 10]),
    ...(told === null ? {} : {}),
  };
};
const randomDetail = () => {
  const count = pick([0, 1, 2, 3, 4]);
  const required = pick([undefined, false, true]);
  const checks = Array.from({ length: count }, (_, index) =>
    check(names[index % names.length], pick(statuses), chance(0.5) ? required : undefined),
  );
  const own = pick([undefined, null, "agent-user"]);
  return {
    headSha: pick(heads),
    checks,
    mergeability: pick(["mergeable", "conflicting", "unknown"]),
    ...(own === undefined ? {} : { viewer: own }),
    author: chance(0.2) ? null : { login: pick(logins), name: null, avatarUrl: null },
  };
};
const randomRemarks = () => {
  if (chance(0.15)) return null;
  return Array.from({ length: pick([0, 0, 1, 1, 2, 3]) }, (_, index) => remark(`r${index}`));
};

const rows = [];
const add = (watch, detail, remarks) => {
  const report = evaluatePullRequestWatch(watch, detail, remarks);
  const row = { watch, detail, remarks, report };
  if (report.changes.length > 0) {
    row.baseBranch = pick(["main", "release/1"]);
    row.message = pullRequestWatchMessage({
      number: 17,
      url: "https://github.com/o/r/pull/17",
      baseBranch: row.baseBranch,
      headSha: report.next.headSha,
      report,
    });
  }
  rows.push(row);
};

// The sequence a watch lives through: each observation is judged against the last one's record.
const initial = (overrides = {}) => ({
  startedAt: STARTED,
  headSha: null,
  failedChecks: [],
  passed: false,
  passedChecks: [],
  remarksThrough: STARTED,
  remarkIds: [],
  conflicting: false,
  wakes: 0,
  ...overrides,
});
for (let run = 0; run < 90; run += 1) {
  let watch = initial();
  for (let step = 0; step < 6; step += 1) {
    const detail = randomDetail();
    const remarks = randomRemarks();
    const report = evaluatePullRequestWatch(watch, detail, remarks);
    add(watch, detail, remarks);
    watch = report.next;
  }
}
// Arbitrary states against arbitrary observations.
for (let index = 0; index < 700; index += 1) add(randomWatch(), randomDetail(), randomRemarks());
// Many remarks at once (only the first ten are listed), one of them long.
add(
  initial({ headSha: "aaaaaaaaaa" }),
  {
    headSha: "aaaaaaaaaa",
    checks: [],
    mergeability: "mergeable",
    viewer: "agent-user",
    author: null,
  },
  Array.from({ length: 12 }, (_, index) => ({
    ...remark(`many${index}`),
    createdAt: "2026-10-02T12:05:00.000Z",
    editedAt: undefined,
    body: index === 0 ? "x".repeat(300) : `note ${index}`,
  })),
);
// A comment bot that rewrites one summary, at the boundary second, and the wake limit.
for (let wakes = 0; wakes <= PULL_REQUEST_WATCH_WAKE_LIMIT; wakes += 1)
  add(
    initial({ wakes, headSha: "aaaaaaaaaa" }),
    {
      headSha: "aaaaaaaaaa",
      checks: [],
      mergeability: "mergeable",
      viewer: "agent-user",
      author: { login: "agent-user", name: null, avatarUrl: null },
    },
    [
      remark("r0"),
      {
        ...remark("r1"),
        createdAt: STARTED,
        editedAt: "2026-10-02T12:30:00.000Z",
        author: { login: "bot[bot]", name: null, avatarUrl: null },
      },
    ],
  );

writeFileSync(
  new URL("../crates/server/tests/fixtures/pull-request-watch.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(
  JSON.stringify({
    cases: rows.length,
    woke: rows.filter((row) => row.report.changes.length > 0).length,
    exhausted: rows.filter((row) => row.report.exhausted).length,
  }),
);
