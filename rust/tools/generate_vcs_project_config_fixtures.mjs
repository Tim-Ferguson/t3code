// Runs the original VcsProjectConfig.ts against real directories and records the kind it
// resolves for each config file content.
import { mkdtempSync, mkdirSync, writeFileSync, renameSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Logger from "../../apps/server/node_modules/effect/dist/Logger.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as VcsProjectConfig from "../../apps/server/src/vcs/VcsProjectConfig.ts";

const layer = VcsProjectConfig.layer.pipe(
  Layer.provide(NodeServices.layer),
  Layer.provideMerge(NodeServices.layer),
);
const contents = [
  '{"vcs":{"kind":"jj"}}',
  '{"vcs":{"kind":"git"}}',
  '{"vcs":{"kind":"unknown"}}',
  '{"vcsKind":"jj"}',
  '{"vcsKind":"git"}',
  '{"vcs":{"kind":"git"},"vcsKind":"jj"}',
  '{"vcs":{},"vcsKind":"jj"}',
  '{"vcs":{"kind":"bogus"},"vcsKind":"jj"}',
  '{"vcs":{"kind":"jj"},"vcsKind":"bogus"}',
  '{"vcsKind":"bogus"}',
  '{"vcs":{"kind":"auto"}}',
  '{"vcs":{"kind":null}}',
  '{"vcs":null}',
  '{"vcs":[]}',
  '{"vcs":"jj"}',
  '{"vcsKind":null}',
  '{"vcsKind":1}',
  "{}",
  "[]",
  '"jj"',
  "7",
  "null",
  "",
  "   ",
  "{not json",
  '{"vcs":{"kind":"jj"}} trailing',
  '{"vcs":{"kind":"jj"},"extra":{"a":[1,2]}}',
  '// line comment\n{"vcs":{"kind":"jj"}}',
  '/* block\ncomment */{"vcs":{"kind":"jj"}}',
  '{"vcs":{"kind":"jj",},}',
  '{"vcs":{"kind":"jj"} , }',
  '{"vcs":{"kind":"jj"}, // trailing comment\n}',
  '{"note":"// not a comment, /* nor this */, ]","vcs":{"kind":"jj"}}',
  '{"note":"a,]","vcsKind":"git"}',
  '{"note":"quote \\" // still string","vcsKind":"git"}',
  '{"vcs":{"kind":"jj"}} // eof comment',
  '{"vcs":{"kind":"jj"}} /* unterminated',
  '﻿{"vcs":{"kind":"jj"}}',
  '{"vcsKind":"git"}\n// c\n',
  '{"vcs":{"kind":"jj"/*x*/}}',
  '{"a":[1,2,],"vcsKind":"jj"}',
  '{"vcsKind":"j\\u006a"}',
  "{'vcsKind':'jj'}",
];
const rows = [];
for (const content of contents) {
  const root = mkdtempSync(join(tmpdir(), "t3-vcs-config-fixture-"));
  mkdirSync(join(root, ".t3code"));
  mkdirSync(join(root, "packages", "app"), { recursive: true });
  writeFileSync(join(root, ".t3code", "vcs.json"), content);
  const warnings = [];
  const kind = await Effect.runPromise(
    Effect.gen(function* () {
      const config = yield* VcsProjectConfig.VcsProjectConfig;
      return yield* config.resolveKind({ cwd: join(root, "packages", "app") });
    }).pipe(
      Effect.provide(layer),
      Effect.provide(Logger.layer([Logger.make(({ message }) => warnings.push(message))])),
    ),
  );
  rmSync(root, { recursive: true, force: true });
  rows.push({ content, kind, warned: warnings.length > 0 });
}
const target = new URL("../crates/vcs/tests/fixtures/vcs-project-config.jsonl", import.meta.url);
const temporary = new URL(target.href + ".tmp");
writeFileSync(temporary, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(temporary, target);
console.log(JSON.stringify({ cases: rows.length }));
