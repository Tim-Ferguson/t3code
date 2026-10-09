// Runs the original helpers behind the lifecycle words and the workspace choice and records what
// they produce: how a confirmation message becomes a title and a description
// (`resolveConfirmDialogCopy`), which worktree is the "previous" one (`resolvePreviousWorktreeSeed`),
// and what the background activity policy says the Git fetch interval is. The Rust tests in
// t3-client replay the rows and require identical output. The two UI modules cannot be imported by
// node (React, extension-less imports), so their functions are cut out of the source and run as
// they stand.
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = new URL("../../", import.meta.url).pathname;
const read = (path) => readFileSync(join(root, path), "utf8");
const cut = (source, name) => {
  const start = source.search(new RegExp(`(export )?function ${name}\\b`));
  if (start < 0) throw new Error(`no ${name}`);
  const next = source.indexOf("\n}\n", start);
  return source.slice(start, next + 3).replace(/^export /, "");
};
const directory = mkdtempSync(join(tmpdir(), "lifecycle-"));
const strip = (code) => code.replace(/\bconst ([A-Za-z_]+):[^=]+=/g, "const $1 =");

const dialog = read("apps/web/src/components/ConfirmDialogHost.tsx");
writeFileSync(
  join(directory, "copy.ts"),
  `type ConfirmationCopy = { readonly title: string; readonly description: string | null };\n${cut(dialog, "resolveConfirmDialogCopy")}\nexport { resolveConfirmDialogCopy };\n`,
);
const { resolveConfirmDialogCopy } = await import(pathToFileURL(join(directory, "copy.ts")));

const toolbar = read("apps/web/src/components/BranchToolbar.logic.ts");
writeFileSync(
  join(directory, "previous.ts"),
  `export interface PreviousWorktreeSeed { branch: string | null; worktreePath: string }\n${cut(read("packages/client-runtime/src/state/threadSort.ts"), "toSortableTimestamp")}\n${cut(toolbar, "resolvePreviousWorktreeSeed")}\n${cut(toolbar, "resolvePreviousWorktreeLabel")}\nexport { toSortableTimestamp, resolvePreviousWorktreeSeed, resolvePreviousWorktreeLabel };\n`,
);
const previous = await import(pathToFileURL(join(directory, "previous.ts")));
const activity = await import(
  pathToFileURL(join(root, "packages/shared/src/backgroundActivitySettings.ts"))
);

const rows = [];
const messages = [
  'Delete thread "Fix it"?\nThis permanently clears conversation history for this thread.',
  'Archive thread "a"?',
  'Remove project "Web" and delete its 2 threads?\nPath: /work/web\nThis permanently clears conversation history for those threads and any archived threads.\nThis removes only this project entry.\nThis action cannot be undone.',
  'Remove project "Web"?\nPath: /work/web\nEnvironment: Laptop\nThis permanently clears any archived conversation history.\nThis removes only this project entry.',
  "No question here",
  "Really? Yes, really.",
  "  ",
  "",
  "First line\nSecond line?\nThird line",
];
for (const message of messages)
  rows.push({ type: "confirm", message, copy: resolveConfirmDialogCopy(message) });

const at = (minutes) => new Date(Date.UTC(2026, 0, 1, 0, minutes)).toISOString();
const thread = (branch, worktreePath, minutes, archivedAt) => ({
  branch,
  worktreePath,
  updatedAt: at(minutes),
  ...(archivedAt === undefined ? {} : { archivedAt }),
});
const sets = [
  { threads: [], current: null },
  {
    threads: [thread("a", "/w/a", 10), thread("b", "/w/b", 30), thread("d", "/w/d", 20)],
    current: null,
  },
  {
    threads: [thread("a", "/w/a", 10), thread("b", "/w/b", 30), thread("d", "/w/d", 20)],
    current: "/w/b",
  },
  { threads: [thread("c", "/w/c", 50, at(1)), thread("x", null, 90)], current: null },
  { threads: [thread(null, "/w/n", 5)], current: "/w/other" },
  { threads: [thread("same", "/w/s", 7), thread("later", "/w/l", 7)], current: null },
];
for (const input of sets) {
  const seed = previous.resolvePreviousWorktreeSeed({
    threads: input.threads,
    currentWorktreePath: input.current,
  });
  rows.push({
    type: "previous",
    input,
    seed,
    label: seed ? previous.resolvePreviousWorktreeLabel(seed) : null,
  });
}

const seconds = (duration) => Math.round(Number(duration.pipe ? 0 : 0) || 0);
const millis = (duration) => {
  // Effect durations are branded values; the module's own conversion gives the milliseconds.
  return Number(JSON.stringify(duration).match(/"millis":(\d+)/)?.[1] ?? NaN);
};
void seconds;
const policies = [
  { schemaVersion: 1, profile: "balanced", overrides: {} },
  { schemaVersion: 1, profile: "performance", overrides: {} },
  { schemaVersion: 1, profile: "battery-saver", overrides: {} },
  { schemaVersion: 1, profile: "custom", baseProfile: "performance", overrides: {} },
  {
    schemaVersion: 1,
    profile: "custom",
    baseProfile: "battery-saver",
    overrides: { idleClientTtl: 1000 },
  },
];
import { createRequire } from "node:module";
const Duration = await import(
  pathToFileURL(
    createRequire(join(root, "packages/shared/package.json")).resolve("effect/Duration"),
  )
).catch(() => null);
for (const policy of policies) {
  const resolved = activity.resolveBackgroundActivitySettings(policy);
  const interval = resolved.automaticGitFetchInterval;
  const ms = Duration ? Duration.toMillis(interval) : millis(interval);
  rows.push({
    type: "fetch",
    policy,
    seconds: Math.round(ms / 1000),
    preset: Math.round(
      (Duration
        ? Duration.toMillis(
            activity.getBackgroundActivityPresetSettings(
              activity.getBackgroundActivityBaseProfile(policy),
            ).automaticGitFetchInterval,
          )
        : 0) / 1000,
    ),
  });
}

writeFileSync(
  join(root, "rust/crates/client/tests/fixtures/lifecycle_logic.json"),
  `${JSON.stringify(rows, null, 1)}\n`,
);
console.log(`${rows.length} rows`);
