// Execute the unchanged alias resolver with a nonconnecting SSH test spawner.
import { writeFileSync } from "node:fs";
import * as Net from "node:net";
import * as Effect from "../../packages/contracts/node_modules/effect/dist/Effect.js";
import * as Sink from "../../packages/contracts/node_modules/effect/dist/Sink.js";
import * as Stream from "../../packages/contracts/node_modules/effect/dist/Stream.js";
import * as ChildProcessSpawner from "../../packages/contracts/node_modules/effect/dist/process/ChildProcessSpawner.js";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import {
  isLocalSshDeviceHost,
  LocalDeviceHostAddresses,
} from "../../apps/server/src/device/localSshDeviceHost.ts";
const rows = [];
for (const hostname of [
  "127.0.1.1",
  "127.00.0.1",
  "127.0.0.256",
  "::1",
  "0:0:0:0:0:0:0:1",
  "::ffff:127.0.0.1",
  "::1%lo0",
  "fe80::1%en0",
  "fe80::1%a:b.c-3",
  "::1%",
  "::1%bad_",
  "[::1]",
  "192.0.2.1",
  "100.65.180.100",
  "2001:db8::1",
]) {
  rows.push({ op: "ip", hostname, result: Net.isIP(hostname) !== 0 });
}
const local = ["100.65.180.100", "fe80::1%en0"];
const configs = [];
for (const hostname of [
  "127.0.1.1",
  "::1",
  "[::1]",
  "[::1",
  "::1]",
  "0:0:0:0:0:0:0:1",
  "192.0.2.1",
  "100.65.180.100",
  "::ffff:127.0.0.1",
  "::1%lo0",
  "fe80::1%en0",
  "2001:db8::1",
])
  for (const suffix of [
    "",
    "proxyjump none\n",
    "proxycommand none\n",
    "proxyjump bastion\n",
    "proxycommand nc remote 22\n",
    "proxyjump NONE\n",
    "proxyjump \n",
    "port 2222\n",
    "port 022\n",
    "port 22\n",
    "hostname 192.0.2.2\n",
  ])
    configs.push(`hostname ${hostname}\nport 22\n${suffix}`);
configs.push(
  "",
  "hostname ::1\n",
  "Hostname ::1\nport 22\n",
  "hostname ::1\nport\t22\n",
  "hostname \ufeff::1\ufeff\nport 22\n",
  "hostname \u0085::1\nport 22\n",
  "hostname \nport 22\n",
  "hostnameX\nport 22\n",
  "hostname🚀\nport 22\n",
);
for (const stdout of configs) {
  // Invalid/nonliteral names would invoke real DNS. Those parse cases are
  // represented by witnesses whose source rejects before DNS.
  if (stdout.includes("\u0085") || stdout.includes("hostnameX")) continue;
  const spawner = ChildProcessSpawner.make((command) =>
    Effect.sync(() => {
      if (command._tag !== "StandardCommand" || !command.args.includes("-G"))
        throw new Error("Unexpected connecting command");
      return ChildProcessSpawner.makeHandle({
        pid: ChildProcessSpawner.ProcessId(123),
        stdout: Stream.make(new TextEncoder().encode(stdout)),
        stderr: Stream.empty,
        all: Stream.empty,
        exitCode: Effect.succeed(ChildProcessSpawner.ExitCode(0)),
        isRunning: Effect.succeed(false),
        kill: () => Effect.void,
        stdin: Sink.drain,
        getInputFd: () => Sink.drain,
        getOutputFd: () => Stream.empty,
        unref: Effect.succeed(Effect.void),
      });
    }),
  );
  const result = await Effect.runPromise(
    isLocalSshDeviceHost({ id: "fixture", label: "Fixture", target: "fixture" }).pipe(
      Effect.provideService(ChildProcessSpawner.ChildProcessSpawner, spawner),
      Effect.provideService(LocalDeviceHostAddresses, new Set(local)),
      Effect.provide(NodeServices.layer),
    ),
  );
  rows.push({ op: "resolve", stdout, local, result });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/device-host-resolver.jsonl", import.meta.url),
  rows.map((row) => JSON.stringify(row)).join("\n") + "\n",
);
console.log(`${rows.length} original resolver/IP witnesses`);
