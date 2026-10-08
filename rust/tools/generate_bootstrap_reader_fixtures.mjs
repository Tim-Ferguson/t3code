// Development-only probes of the unchanged reader and its read-error mapping.
import fs from "node:fs";
import readline from "node:readline";
import { syncBuiltinESMExports } from "node:module";
import { Readable } from "node:stream";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as Effect from "../../packages/contracts/node_modules/effect/dist/Effect.js";
import * as Schema from "../../packages/contracts/node_modules/effect/dist/Schema.js";
import * as Option from "../../packages/contracts/node_modules/effect/dist/Option.js";
import { readBootstrapEnvelope } from "../../apps/server/src/bootstrap.ts";
import { HostProcessPlatform } from "../../packages/shared/src/hostProcess.ts";
const rows = [],
  root = mkdtempSync(join(tmpdir(), "t3-bootstrap-oracle-"));
const schema = Schema.Struct({ mode: Schema.String });
const original = fs.createReadStream;
const originalInterface = readline.createInterface;
// Node24 readline forwards input errors to Interface. Catch that harness-only
// event so the unchanged source stream listener can expose its intended mapping.
readline.createInterface = (...args) => originalInterface(...args).on("error", () => {});
syncBuiltinESMExports();
for (const input of [
  Buffer.from('{"mode":"desktop"}\n'),
  Buffer.from('{"mode":"desktop"}\r\n'),
  Buffer.from('{"mode":"desktop"}\r'),
  Buffer.from('{"mode":"desktop"}'),
  Buffer.from(""),
  Buffer.from('{"mode":42}\n'),
  Buffer.from("[]\n"),
  Buffer.from('\n{"mode":"desktop"}'),
  Buffer.from('{"mode":"desktop"}\nignored'),
  Buffer.from('{"mode":"\ufeff"}\n'),
  Buffer.from('\ufeff{"mode":"desktop"}\n'),
  Buffer.concat([Buffer.from('{"mode":"'), Buffer.from([0xff, 0xc0, 0xaf]), Buffer.from('"}\n')]),
]) {
  const path = join(root, "input");
  writeFileSync(path, input);
  const fd = fs.openSync(path, "r");
  try {
    const result = await Effect.runPromise(readBootstrapEnvelope(schema, fd));
    rows.push({
      op: "line",
      input: input.toString("hex"),
      ...(Option.isSome(result) ? { result: result.value } : { result: null }),
    });
  } catch (error) {
    rows.push({ op: "line", input: input.toString("hex"), error: error._tag });
  } finally {
    try {
      fs.closeSync(fd);
    } catch {}
  }
}
for (const code of ["EBADF", "ENOENT", "EACCES", "EIO"]) {
  const path = join(root, "input");
  writeFileSync(path, "unused");
  const fd = fs.openSync(path, "r");
  fs.createReadStream = (_path, options) =>
    new Readable({
      read() {
        this.destroy(Object.assign(new Error("read failed"), { code }));
      },
      destroy(error, done) {
        try {
          fs.closeSync(options.fd);
        } catch {}
        done(error);
      },
    });
  syncBuiltinESMExports();
  try {
    const result = await Effect.runPromise(
      readBootstrapEnvelope(schema, fd).pipe(Effect.provideService(HostProcessPlatform, "darwin")),
    );
    rows.push({ op: "read-error", code, result: Option.isSome(result) ? result.value : null });
  } catch (error) {
    rows.push({
      op: "read-error",
      code,
      error: error._tag,
      message: error.message.replaceAll(String(fd), "7"),
      causeCode: error.cause.code,
    });
  } finally {
    fs.createReadStream = original;
    syncBuiltinESMExports();
    try {
      fs.closeSync(fd);
    } catch {}
  }
}
readline.createInterface = originalInterface;
syncBuiltinESMExports();
rmSync(root, { recursive: true, force: true });
writeFileSync(
  new URL("../crates/server/tests/fixtures/bootstrap-reader.jsonl", import.meta.url),
  rows.map((x) => JSON.stringify(x)).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
