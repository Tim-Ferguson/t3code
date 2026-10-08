// Original Effect framing pipeline used by NativeTelemetryClient; no sidecar.
import { writeFileSync } from "node:fs";
import { pathToFileURL, fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url));
const base = root + "packages/contracts/node_modules/effect/dist/";
const [Stream, Channel, Effect] = await Promise.all(
  ["Stream", "Channel", "Effect"].map((name) => import(pathToFileURL(base + name + ".js"))),
);
const inputs = [
  [
    239,
    187,
    191,
    ...Buffer.from('{"text":"😀"}\r\n\n \r{"invalid":"'),
    255,
    ...Buffer.from('"}\n{}'),
  ],
  [...Buffer.from("a\rb\r\nc\nd\r")],
  [255, 120, 226, 130],
  [226, 130],
  [...Buffer.from("\n\ufeff{}")],
  [...Buffer.from("\ufeff\ufeff{}")],
  [...Buffer.from("{}\r\r\n{}\n\r\n")],
];
const fixtures = [];
for (const bytes of inputs)
  for (let size = 1; size <= bytes.length + 1; size++) {
    const chunks = [];
    for (let at = 0; at < bytes.length; at += size) chunks.push(bytes.slice(at, at + size));
    const lines = await Effect.runPromise(
      Stream.runCollect(
        Stream.fromIterable(
          chunks.map((chunk) => new Uint8Array(chunk)),
          { chunkSize: 1 },
        ).pipe(
          Stream.pipeThroughChannel(Channel.decodeText()),
          Stream.pipeThroughChannel(Channel.splitLines()),
        ),
      ),
    );
    fixtures.push({ chunks, lines });
  }
writeFileSync(
  new URL("../crates/server/tests/fixtures/resource-ndjson.jsonl", import.meta.url),
  fixtures.map((f) => JSON.stringify(f)).join("\n") + "\n",
);
console.log(`${fixtures.length} original Effect TextDecoder/splitLines witnesses`);
