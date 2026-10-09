// Development oracle for `shell.openInEditor`: drives the original `ExternalLauncher` with a
// recording spawner over a throwaway PATH of stub executables, and records the command and
// arguments of every launch (and the editors it finds). The Rust launcher replays each row over
// the same PATH and must build the same command line. Run:
//   node rust/tools/generate_external_launcher_fixtures.mjs
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  realpathSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as NodeServices from "../../apps/server/node_modules/@effect/platform-node/dist/NodeServices.js";
import * as ConfigProvider from "../../apps/server/node_modules/effect/dist/ConfigProvider.js";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as Sink from "../../apps/server/node_modules/effect/dist/Sink.js";
import * as Stream from "../../apps/server/node_modules/effect/dist/Stream.js";
import { ChildProcessSpawner } from "../../apps/server/node_modules/effect/dist/process/index.js";
import { EDITORS } from "../../packages/contracts/src/index.ts";
import { HostProcessPlatform } from "../../packages/shared/src/hostProcess.ts";
import { SpawnExecutableResolution } from "../../packages/shared/src/shell.ts";
import * as ExternalLauncher from "../../apps/server/src/process/externalLauncher.ts";

const base = realpathSync(mkdtempSync(join(realpathSync(tmpdir()), "t3-launcher-")));
const bin = join(base, "bin");
const home = join(base, "home");
mkdirSync(bin);
mkdirSync(home);
const stub = (name, body = "") => {
  writeFileSync(join(bin, name), `#!/bin/sh\n${body}\n`);
  chmodSync(join(bin, name), 0o755);
};
const stubbed = ["xdg-open", "xdg-mime", "open"];
for (const editor of EDITORS) if (editor.commands) stubbed.push(editor.commands[0]);
for (const name of stubbed)
  stub(name, name === "xdg-mime" ? "echo org.gnome.Nautilus.desktop" : "");

const handle = () =>
  ChildProcessSpawner.makeHandle({
    pid: ChildProcessSpawner.ProcessId(1),
    exitCode: Effect.succeed(ChildProcessSpawner.ExitCode(0)),
    isRunning: Effect.succeed(true),
    kill: () => Effect.void,
    unref: Effect.sync(() => Effect.void),
    stdin: Sink.drain,
    stdout: Stream.make(new TextEncoder().encode("org.gnome.Nautilus.desktop\n")),
    stderr: Stream.empty,
    all: Stream.empty,
    getInputFd: () => Sink.drain,
    getOutputFd: () => Stream.empty,
  });

const run = (platform, env, program) => {
  const spawned = [];
  const layer = Layer.mergeAll(
    ExternalLauncher.layer.pipe(
      Layer.provide(
        Layer.merge(
          NodeServices.layer,
          Layer.succeed(
            ChildProcessSpawner.ChildProcessSpawner,
            ChildProcessSpawner.make((command) =>
              Effect.sync(
                () => (
                  spawned.push({ command: command.command, args: [...command.args] }),
                  handle()
                ),
              ),
            ),
          ),
        ),
      ),
    ),
    Layer.succeed(HostProcessPlatform, platform),
    Layer.succeed(SpawnExecutableResolution, (command) => command),
    ConfigProvider.layer(ConfigProvider.fromEnv({ env })),
  );
  return Effect.runPromise(
    Effect.gen(function* () {
      const launcher = yield* ExternalLauncher.ExternalLauncher;
      return yield* program(launcher);
    }).pipe(Effect.scoped, Effect.provide(layer), Effect.provide(NodeServices.layer)),
  ).then((value) => ({ value, spawned }));
};

const hide = (value) => JSON.parse(JSON.stringify(value).split(bin).join("{bin}"));
const rows = [];
const cwds = [
  "/work/project",
  "/work/a b/c.ts",
  "/work/c.ts:12",
  "/work/c.ts:12:4",
  "/work/c.ts:abc",
  "/work/c.ts:1:2:3",
  ":1:2",
  "/work/dir:7",
];
for (const platform of ["linux", "darwin"]) {
  const environments = [
    { name: "plain", env: { PATH: bin, HOME: home } },
    { name: "display", env: { PATH: bin, HOME: home, DISPLAY: ":0" } },
    { name: "wayland", env: { PATH: bin, HOME: home, WAYLAND_DISPLAY: "wayland-0" } },
    {
      name: "wsl-graphical",
      env: { PATH: bin, HOME: home, DISPLAY: ":0", WSL_DISTRO_NAME: "Ubuntu" },
    },
    {
      name: "wsl-ssh",
      env: {
        PATH: bin,
        HOME: home,
        DISPLAY: ":0",
        WSL_DISTRO_NAME: "Ubuntu",
        SSH_TTY: "/dev/pts/1",
      },
    },
  ];
  for (const { name, env } of environments) {
    for (const editor of EDITORS) {
      for (const cwd of editor.commands ? cwds : ["/work/project", "/work/a b/file.txt"]) {
        for (const reveal of editor.commands ? [undefined] : [undefined, false, true]) {
          const input = { editor: editor.id, cwd, ...(reveal === undefined ? {} : { reveal }) };
          const outcome = await run(platform, env, (launcher) =>
            launcher.launchEditor(input).pipe(Effect.exit),
          );
          const exit = outcome.value;
          const failure =
            exit._tag === "Failure"
              ? (exit.cause.reasons?.[0]?.error ?? exit.cause.failures?.[0]?.error)
              : null;
          rows.push({
            platform,
            environment: name,
            input,
            result:
              exit._tag === "Success"
                ? { spawned: hide(outcome.spawned.filter((call) => call.command !== "xdg-mime")) }
                : {
                    error: failure?._tag ?? "unknown",
                    detail: hide(
                      failure ? { editor: failure.editor, command: failure.command } : null,
                    ),
                  },
          });
        }
      }
    }
  }
}
// What discovery finds on PATH alone, and the reveal kind per environment.
const discovery = [];
for (const platform of ["linux", "darwin"]) {
  for (const env of [
    { PATH: bin, HOME: home },
    { PATH: bin, HOME: home, DISPLAY: ":0" },
    { PATH: bin, HOME: home, DISPLAY: ":0", WSL_DISTRO_NAME: "Ubuntu" },
    { PATH: "", HOME: home },
  ]) {
    const outcome = await run(platform, env, (launcher) =>
      Effect.all([launcher.resolveAvailableEditors(), launcher.resolveFileManagerRevealKind()]),
    );
    discovery.push({
      platform,
      env: hide(env),
      available: outcome.value[0],
      revealKind: outcome.value[1] ?? null,
    });
  }
}
const target = new URL("../crates/server/tests/fixtures/external-launcher.json", import.meta.url);
writeFileSync(
  new URL(target.href + ".tmp"),
  JSON.stringify(
    { stubbed, editors: EDITORS.map((editor) => editor.id), rows, discovery },
    null,
    1,
  ),
);
renameSync(new URL(target.href + ".tmp"), target);
rmSync(base, { recursive: true, force: true });
console.log(JSON.stringify({ rows: rows.length, discovery: discovery.length }));
