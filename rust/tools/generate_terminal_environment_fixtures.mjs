// Pure policy from the original terminal manager; no provider or PTY startup.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import path from "node:path";
const source = readFileSync(
  new URL("../../apps/server/src/terminal/Manager.ts", import.meta.url),
  "utf8",
);
const expansion = readFileSync(
  new URL("../../apps/server/src/pathExpansion.ts", import.meta.url),
  "utf8",
);
const expandSource = expansion.slice(
  expansion.indexOf("export function expandHomePath("),
  expansion.indexOf("/**", expansion.indexOf("export function expandHomePath(")),
);
const shellSource = source.slice(
  source.indexOf("function defaultShellResolver"),
  source.indexOf("function isRetryableShellSpawnError"),
);
const envSource = source.slice(
  source.indexOf("function shouldExcludeTerminalEnvKey"),
  source.indexOf("function normalizedRuntimeEnv"),
);
let currentHome = "/fixture/home",
  currentPlatform = "darwin";
const NodeOS = { homedir: () => currentHome };
const NodePath = {
  join: (...parts) => (currentPlatform === "win32" ? path.win32 : path.posix).join(...parts),
};
const pure = stripTypeScriptTypes(
  "const TERMINAL_ENV_BLOCKLIST=new Set(['PORT','ELECTRON_RENDERER_PORT','ELECTRON_RUN_AS_NODE']);\n" +
    expandSource +
    shellSource +
    envSource,
  { mode: "strip" },
).replaceAll("export function ", "function ");
const policy = new Function(
  "NodeOS",
  "NodePath",
  pure + "\nreturn {resolveShellCandidates,defaultShellResolver,createTerminalSpawnEnv};",
)(NodeOS, NodePath);
const shells = [];
for (const platform of ["darwin", "linux", "win32"]) {
  for (const requested of [
    null,
    "",
    "  /bin/zsh -l  ",
    '\ufeff"/bin/bash" -l\ufeff',
    "'zsh'",
    "\u0085zsh",
    "C:\\custom shell\\pwsh.exe",
    "pwsh.exe",
    '""',
    " / ",
  ]) {
    for (const env of [
      {},
      { SHELL: "/bin/zsh -l" },
      { SystemRoot: "C:/Windows/", ComSpec: "C:\\Windows\\System32\\cmd.exe" },
      { SystemRoot: "  ", windir: " D:\\WIN ", ComSpec: "" },
      { SystemRoot: "C:/root/../WIN\\", ComSpec: "C:\\mixed/../cmd.exe" },
    ]) {
      const result = policy.resolveShellCandidates(
        () => requested ?? policy.defaultShellResolver(platform, env),
        platform,
        env,
      );
      shells.push({ platform, requested, env, result });
    }
  }
}
const environments = [];
const cases = [
  [{}, null],
  [
    {
      PORT: "1",
      T3CODE_PORT: "2",
      t3code_key: "x",
      VITE_FOO: "x",
      Electron_Run_As_Node: "1",
      KEEP: "keep",
    },
    null,
  ],
  [{ COLORTERM: "24bit" }, { COLORTERM: "" }],
  [{ COLORTERM: "" }, {}],
  [
    { Path: "one", PATH: "two" },
    { path: "three", PORT: "explicit", T3CODE_OWN: "explicit" },
  ],
  [
    { Ä_VAR: "old", K_VAR: "old" },
    { ä_var: "new", k_var: "new" },
  ],
  [{}, { CODEX_HOME: "~/.codex", CLAUDE_CONFIG_DIR: "~\\.claude", CUSTOM: "~/unchanged" }],
  [{}, { CODEX_HOME: "~/../outside/./dir", CLAUDE_CONFIG_DIR: "~" }],
  [{}, { CODEX_HOME: "~/../../../../above", CLAUDE_CONFIG_DIR: "~/../../.." }],
  [{}, { CODEX_HOME: "~/foo/", CLAUDE_CONFIG_DIR: "~\\foo\\" }],
  [{}, { CODEX_HOME: "~/", CLAUDE_CONFIG_DIR: "~//" }],
  [
    {
      APPIMAGE: "image",
      APPDIR: "/tmp/mount///",
      ARGV0: "arg",
      OWD: "cwd",
      PATH: "/tmp/mount/bin:/tmp/mount:/tmp/mount-other/bin::/bin",
      LD_LIBRARY_PATH: "/tmp/mount/lib",
      XDG_DATA_DIRS: "/user/share:/tmp/mount/share",
    },
    null,
  ],
  [{ APPDIR: "/", PATH: "/:/bin::/usr/bin", OWD: "remove" }, null],
  [{ PATH: "::one::two", OWD: "keep" }, null],
  [{ APPIMAGE: "image", ARGV0: "arg", OWD: "cwd", PATH: "::keep" }, null],
];
for (const platform of ["darwin", "linux", "win32"]) {
  currentPlatform = platform;
  currentHome = platform === "win32" ? "C:\\Users\\Fixture" : "/fixture/home";
  for (const [base, runtime] of cases) {
    const result = policy.createTerminalSpawnEnv(base, runtime, platform);
    environments.push({ platform, home: currentHome, base, runtime, result });
  }
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/terminal-environment.json", import.meta.url),
  JSON.stringify({ shells, environments }) + "\n",
);
console.log(`${shells.length}shell policies, ${environments.length}environment policies`);
