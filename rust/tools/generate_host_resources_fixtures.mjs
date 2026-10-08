import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const Effect = await import(
  new URL("../../packages/contracts/node_modules/effect/dist/Effect.js", import.meta.url)
);
const MutableHashMap = await import(
  new URL("../../packages/contracts/node_modules/effect/dist/MutableHashMap.js", import.meta.url)
);
const Clock = await import(
  new URL("../../packages/contracts/node_modules/effect/dist/Clock.js", import.meta.url)
);
const Cache = await import(
  new URL("../../packages/contracts/node_modules/effect/dist/Cache.js", import.meta.url)
);
const Fiber = await import(
  new URL("../../packages/contracts/node_modules/effect/dist/Fiber.js", import.meta.url)
);
const Deferred = await import(
  new URL("../../packages/contracts/node_modules/effect/dist/Deferred.js", import.meta.url)
);
const Stream = await import(
  new URL("../../packages/contracts/node_modules/effect/dist/Stream.js", import.meta.url)
);
const Spawner = await import(
  new URL(
    "../../packages/contracts/node_modules/effect/dist/process/ChildProcessSpawner.js",
    import.meta.url,
  )
);
const TestClock = await import(
  new URL("../../packages/contracts/node_modules/effect/dist/testing/TestClock.js", import.meta.url)
);
let calls = 0,
  cancels = 0,
  wall = 0;
Date.now = () => wall;
const result = await Effect.runPromise(
  Effect.provide(
    Effect.gen(function* () {
      const started = yield* Deferred.make();
      const gate = yield* Deferred.make();
      const cache = yield* Cache.make({
        capacity: 1,
        timeToLive: "5 seconds",
        lookup: () =>
          Effect.gen(function* () {
            calls++;
            yield* Deferred.succeed(started, undefined);
            yield* Deferred.await(gate);
            yield* Effect.sleep("200 millis");
            return calls;
          }).pipe(Effect.onInterrupt(() => Effect.sync(() => cancels++))),
      });
      const a = yield* Effect.forkChild(Cache.get(cache, "host"));
      yield* Deferred.await(started);
      const b = yield* Effect.forkChild(Cache.get(cache, "host"));
      yield* Effect.yieldNow;
      yield* Fiber.interrupt(a);
      const sharedCalls = calls;
      yield* Deferred.succeed(gate, undefined);
      wall = 200;
      yield* TestClock.adjust("200 millis");
      const value = yield* Fiber.join(b);
      yield* Effect.yieldNow;
      wall = 5199;
      yield* TestClock.adjust("4999 millis");
      const before = yield* Cache.get(cache, "host");
      wall = 5200;
      yield* TestClock.adjust("1 millis");
      const now = yield* Clock.currentTimeMillis;
      const expires = [...MutableHashMap.values(cache.map)].map((e) => e.expiresAt);
      const after = yield* Cache.has(cache, "host");
      const next = yield* Effect.forkChild(Cache.get(cache, "host"));
      yield* TestClock.adjust("200 millis");
      yield* Fiber.join(next);
      const separate = yield* Cache.make({
        capacity: 1,
        timeToLive: "5 seconds",
        lookup: () => Effect.never.pipe(Effect.onInterrupt(() => Effect.sync(() => cancels++))),
      });
      const abandoned = yield* Effect.forkChild(Cache.get(separate, "host"));
      yield* Effect.yieldNow;
      yield* Fiber.interrupt(abandoned);
      const retained = yield* Cache.has(separate, "host");
      return { sharedCalls, value, before, calls, cancels, retained, after, now, expires };
    }),
    TestClock.layer(),
  ),
);
const rows = [{ op: "cache", result }];

const source = readFileSync(
  new URL("../../apps/server/src/resourceTelemetry/HostResources.ts", import.meta.url),
  "utf8",
);
const helper = source.slice(
  source.indexOf("function darwinAvailableMemory"),
  source.indexOf("const make ="),
);
const darwin = new Function(stripTypeScriptTypes(helper) + ";return darwinAvailableMemory;")();
const cpuText = source.slice(
  source.indexOf("const totalDelta"),
  source.indexOf("const totalMemoryBytes"),
);
const cpu = new Function("previousCpu", "cpu", cpuText + ";return cpuUtilization;");
const linuxRegex = /^MemAvailable:\s+(\d+)\s+kB$/m;
const base =
  "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free: 10.\nPages inactive: 20.\nPages speculative: 30.\n";
// The original derived string collector does not require a successful exit.
const spawner = Spawner.make(() =>
  Effect.succeed({
    stdout: Stream.fromIterable([new TextEncoder().encode(base)]),
    exitCode: Effect.succeed(7),
  }),
);
const nonzeroOutput = await Effect.runPromise(spawner.string(undefined));
rows.push({
  op: "vm_stat_nonzero",
  input: nonzeroOutput,
  exitCode: 7,
  result: darwin(nonzeroOutput),
});
for (const input of [
  base,
  "",
  base.replace("16384", "0"),
  base.replace("20.", "-20."),
  base.replace("Pages free: 10.", "Pages free: 10"),
  base.replace("30.", "9007199254740991."),
  base.replace("16384", "1"),
  base.replace("free: ", "free:\uFEFF"),
  base.replace("free: ", "free:\u0085"),
  base.replaceAll("\n", "\r\n"),
  base + "Pages purgeable: 1000.",
])
  rows.push({ op: "darwin", input, result: darwin(input) });
for (const input of [
  "",
  "MemAvailable: 123 kB",
  "MemAvailable: 0 kB",
  "MemAvailable: 123 kB\n",
  "MemAvailable: 123 kB\r\n",
  "MemAvailable: 123 kB ",
  "MemAvailable:\uFEFF123 kB",
  "MemAvailable:\u0085123 kB",
  "MemAvailable: 123 MB",
  " MemAvailable: 123 kB",
]) {
  const value = linuxRegex.exec(input)?.[1];
  rows.push({ op: "linux", input, result: value ? Number(value) * 1024 : null });
}
for (const [before, after] of [
  [
    { idle: 100, total: 200, count: 2 },
    { idle: 120, total: 280, count: 2 },
  ],
  [
    { idle: 100, total: 200, count: 2 },
    { idle: 120, total: 280, count: 4 },
  ],
  [
    { idle: 100, total: 200, count: 2 },
    { idle: 0, total: 0, count: 2 },
  ],
  [
    { idle: 100, total: 200, count: 2 },
    { idle: 200, total: 201, count: 2 },
  ],
  [
    { idle: 100, total: 200, count: 2 },
    { idle: 100, total: 200, count: 2 },
  ],
  [
    { idle: 100, total: 200, count: 2 },
    { idle: 99, total: 250, count: 2 },
  ],
  [
    { idle: 0, total: 0, count: 0 },
    { idle: 0, total: 0, count: 0 },
  ],
])
  rows.push({ op: "cpu", before, after, result: cpu(before, after) });
writeFileSync(
  new URL("../crates/server/tests/fixtures/host-resources.jsonl", import.meta.url),
  rows.map((x) => JSON.stringify(x)).join("\n") + "\n",
);
console.log(JSON.stringify({ cases: rows.length }));
