// Runs the original pure helpers behind reactions and rewriting words on a pull request and records
// what they answer: the pill's spoken sentence (`pullRequestReactionTooltip`), a press in flight
// (`applyPendingPullRequestReactions`) and who may rewrite the title or a remark
// (`canEditPullRequestChangeRequest`, `canEditPullRequestComment`). Both modules only import types,
// so node runs them as they stand. The Rust tests in t3-client replay the rows.
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = new URL("../../", import.meta.url).pathname;
const reactions = await import(
  pathToFileURL(join(root, "apps/web/src/components/pullRequest/pullRequestReactions.logic.ts"))
);
const editing = await import(
  pathToFileURL(join(root, "apps/web/src/components/pullRequest/pullRequestEditing.logic.ts"))
);

const rows = [];
const reaction = (content, count, actors, viewerHasReacted) => ({
  content,
  count,
  actors,
  viewerHasReacted,
});
const samples = [
  reaction("thumbs-up", 1, [], true),
  reaction("thumbs-up", 1, ["bob"], false),
  reaction("heart", 2, ["bob"], true),
  reaction("heart", 2, ["bob", "carol"], true),
  reaction("rocket", 5, ["a", "b", "c", "d"], false),
  reaction("rocket", 5, ["a", "b"], true),
  reaction("eyes", 3, [], false),
  reaction("laugh", 2, ["octo", "bob"], true),
  reaction("hooray", 1, [], false),
  reaction("confused", 4, ["x"], true),
];
for (const sample of samples)
  rows.push({
    type: "tooltip",
    reaction: sample,
    text: reactions.pullRequestReactionTooltip(sample),
  });
for (const content of reactions.PULL_REQUEST_REACTION_ORDER)
  rows.push({
    type: "name",
    content,
    emoji: reactions.pullRequestReactionEmoji(content),
    name: reactions.pullRequestReactionName(content),
  });
const base = [
  reaction("thumbs-up", 2, ["bob"], true),
  reaction("heart", 1, ["carol"], false),
  reaction("eyes", 1, [], false),
];
const pendings = [
  [],
  [["thumbs-up", false]],
  [["heart", true]],
  [["rocket", true]],
  [["eyes", false]],
  [
    ["rocket", true],
    ["thumbs-up", false],
    ["heart", true],
  ],
  [["laugh", false]],
  [["thumbs-up", true]],
];
for (const pending of pendings) {
  rows.push({
    type: "pending",
    reactions: base,
    pending,
    shown: reactions.applyPendingPullRequestReactions(base, new Map(pending)),
  });
}

const detail = (viewer, author, actions, edit) => ({
  viewer,
  author: author === null ? null : { login: author },
  viewerPermissions: { actions },
  capabilities: { edit },
});
const subjects = [
  detail("octo", "octo", [], { changeRequest: true, comment: true }),
  detail("Octo", "octo", [], { changeRequest: true, comment: true }),
  detail("octo", "alice", ["merge"], { changeRequest: true, comment: true }),
  detail("octo", "alice", ["close"], { changeRequest: true, comment: true }),
  detail("octo", "octo", ["merge"], { changeRequest: false, comment: true }),
  detail("octo", "octo", ["merge"], { changeRequest: true, comment: false }),
  detail("octo", "octo", ["merge"], undefined),
  detail(null, "octo", ["merge"], { changeRequest: true, comment: true }),
  detail("octo", null, [], { changeRequest: true, comment: true }),
];
const comments = [
  { author: { login: "octo" }, kind: "issue-comment" },
  { author: { login: "OCTO " }, kind: "review-comment" },
  { author: { login: "octo" }, kind: "review" },
  { author: { login: "bob" }, kind: "issue-comment" },
  { author: null, kind: "issue-comment" },
];
for (const subject of subjects) {
  rows.push({
    type: "change-request",
    detail: subject,
    can: editing.canEditPullRequestChangeRequest(subject),
  });
  for (const comment of comments)
    rows.push({
      type: "comment",
      detail: subject,
      comment,
      can: editing.canEditPullRequestComment(subject, comment),
    });
}
writeFileSync(
  join(root, "rust/crates/client/tests/fixtures/pull_request_edit.json"),
  `${JSON.stringify(rows, null, 1)}\n`,
);
console.log(`${rows.length} rows`);
