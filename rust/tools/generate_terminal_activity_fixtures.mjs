// Source-only process-table policy; no live process inspection or child startup.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
const source = readFileSync(
  new URL("../../apps/server/src/terminal/Manager.ts", import.meta.url),
  "utf8",
);
const labels = source.slice(
  source.indexOf("function truncateTerminalWireLabel("),
  source.indexOf("function terminalWireLabel("),
);
const tables = source.slice(
  source.indexOf("interface TerminalProcessTableSnapshot"),
  source.indexOf("const POSIX_PS_ABSOLUTE_PATHS"),
);
const windowsStart = source.indexOf("const processes = result.stdout.split");
const windowsEnd =
  source.indexOf("return processTableSnapshotFromProcesses(processes);", windowsStart) +
  "return processTableSnapshotFromProcesses(processes);".length;
const windows = `function parseWindows(stdout: string) { const result = {stdout}; ${source.slice(windowsStart, windowsEnd)} }`;
const pure = stripTypeScriptTypes(
  `const MAX_TERMINAL_LABEL_LENGTH=128; const MAX_SUBPROCESS_POLL_INTERVAL_MS=60000; ${labels}\n${tables}\n${windows}`,
  { mode: "strip" },
).replaceAll("export function ", "function ");
const policy = new Function(
  pure +
    "\nreturn {normalizeChildCommandName,parsePosixProcessTable,parseWindows,processTableSnapshotFromProcesses,deriveSubprocessInspectResult,subprocessSnapshotPollDelayMs};",
)();
const cases = [];
for (const platform of ["darwin", "linux", "win32"]) {
  for (const raw of [
    "",
    "  ",
    "\ufeff/bin/zsh\ufeff",
    "\u0085zsh",
    " [ /usr/bin/bash ] ",
    "(node --inspect)",
    "[/usr/bin/node]",
    "/bin/my command",
    "C:\\Windows\\System32\\CMD.EXE args",
    "C:/Windows/PWSH.exe",
    "/",
    ".exe",
    "[]",
    "[ ]",
    "vim".repeat(60),
    "前".repeat(130),
    "😀".repeat(64),
    "vim\u2028ignored",
    "x\u0085kept",
  ]) {
    cases.push({
      kind: "normalize",
      raw,
      platform,
      expected: policy.normalizeChildCommandName(raw, platform),
    });
  }
  const tables = [
    [],
    [{ pid: 100, ppid: 1, name: "/bin/zsh" }],
    [
      { pid: 100, ppid: 1, name: "/bin/zsh" },
      { pid: 101, ppid: 100, name: "zsh" },
    ],
    [
      { pid: 100, ppid: 1, name: "/bin/zsh" },
      { pid: 101, ppid: 100, name: "[zsh]" },
      { pid: 102, ppid: 100, name: "/usr/bin/vim" },
    ],
    [
      { pid: 100, ppid: 1, name: "/bin/zsh" },
      { pid: 101, ppid: 100, name: "zsh" },
      { pid: 102, ppid: 101, name: "node" },
      { pid: 103, ppid: 100, name: "python" },
      { pid: 104, ppid: 103, name: "worker" },
    ],
    [{ pid: 101, ppid: 100, name: "" }],
    [
      { pid: 100, ppid: 101, name: "zsh" },
      { pid: 101, ppid: 100, name: "bash" },
      { pid: 101, ppid: 100, name: "vim" },
    ],
    [
      { pid: 100.5, ppid: 1, name: "bad" },
      { pid: 101, ppid: 100, name: "node" },
      { pid: 102, ppid: 100.5, name: "badparent" },
    ],
    [
      { pid: 100, ppid: 1, name: "pwsh.exe" },
      { pid: 101, ppid: 100, name: "C:\\Windows\\PWSH.EXE" },
    ],
    [
      { pid: 100, ppid: 1, name: "sh" },
      { pid: 101, ppid: 100, name: "x".repeat(200) },
    ],
  ];
  for (const entries of tables) {
    cases.push({
      kind: "entries",
      entries,
      pid: 100,
      platform,
      expected: policy.deriveSubprocessInspectResult(
        policy.processTableSnapshotFromProcesses(entries),
        100,
        platform,
      ),
    });
  }
  for (const stdout of [
    " 100 1 /bin/zsh\n101 100 /usr/bin/vim editor\n",
    "100 1 zsh\r\n101 100 zsh\r\n102 101 node\r\n",
    "ignored\n-100 1 bad\n100.0 1 bad\n100 1 sh\n101 100 \n102 100\n103 100 grep\n",
    "\ufeff100\u00a01\u2003zsh\ufeff\n101 100 vim\u2028hidden\n102\t100\t前\n",
    "100 1 sh\n0 100 zero\n000101 000100 node\n9007199254740993 100 rounded\n",
    "100 1 sh\n101 100 vim\n101 100 node\n102 101 worker\n100 102 sh\n",
    "100 1 sh\n101 100 " + "x".repeat(200),
  ])
    cases.push({
      kind: "posix",
      stdout,
      pid: 100,
      platform,
      expected: policy.deriveSubprocessInspectResult(
        policy.parsePosixProcessTable(stdout),
        100,
        platform,
      ),
    });
}
for (const stdout of [
  "100|1|pwsh.exe\r\n101|100|node.exe\r\n",
  "100|1|pwsh.exe\n101|100|PWSH.EXE\n",
  "100|1|pwsh.exe\n101|100|pwsh.exe\n102|101|node.exe\n",
  "100|1|pwsh.exe\n0|100|zero\n-1|100|negative\n101.5|100|fraction\n102|100.5|fractionParent\n",
  "100|1|pwsh.exe\n0x65|1e2|C:\\x\\node.EXE|discarded\n",
  "100|1|cmd.exe\n101||emptyParent\n102|100|\ufeffNode.EXE\ufeff\n",
])
  cases.push({
    kind: "windows",
    stdout,
    pid: 100,
    platform: "win32",
    expected: policy.deriveSubprocessInspectResult(policy.parseWindows(stdout), 100, "win32"),
  });
for (const interval of [0, 1, 1000, 1500, 60000, 100000])
  for (const failures of [0, 1, 2, 6, 30]) {
    cases.push({
      kind: "delay",
      interval,
      failures,
      expected: policy.subprocessSnapshotPollDelayMs(interval, failures),
    });
  }
writeFileSync(
  new URL("../crates/server/tests/fixtures/terminal-activity.jsonl", import.meta.url),
  cases.map((c) => JSON.stringify(c)).join("\n") + "\n",
);
console.log(`${cases.length} source terminal activity witnesses`);
