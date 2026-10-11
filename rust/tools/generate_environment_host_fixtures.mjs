// Development oracle for the machine kind and the label of the environment descriptor (`environment/ServerEnvironmentMachine.ts`,
// `environment/ServerEnvironmentLabel.ts`): the unchanged original runs over scripted hosts (a platform, a hostname, files that
// exist or fail, commands that answer or fail) and its answers are recorded. `tests/environment_host_flow.rs` replays the same
// hosts through the Rust probes. Written to a `.jsonl` file so the formatter leaves it alone.
// Run: node rust/tools/generate_environment_host_fixtures.mjs
import { writeFileSync } from "node:fs";
import * as Effect from "../../apps/server/node_modules/effect/dist/Effect.js";
import * as FileSystem from "../../apps/server/node_modules/effect/dist/FileSystem.js";
import * as Layer from "../../apps/server/node_modules/effect/dist/Layer.js";
import * as ChildProcessSpawner from "../../apps/server/node_modules/effect/dist/process/ChildProcessSpawner.js";
import { HostProcessHostname, HostProcessPlatform } from "../../packages/shared/src/hostProcess.ts";
import * as ProcessRunner from "../../apps/server/src/processRunner.ts";
import {
  detectServerEnvironmentMachineKind,
  machineKindFromAppleProductName,
  machineKindFromDmi,
} from "../../apps/server/src/environment/ServerEnvironmentMachine.ts";
import { resolveServerEnvironmentLabel } from "../../apps/server/src/environment/ServerEnvironmentLabel.ts";

const rows = [];

const run = async (effect, host) => {
  const key = (command, args) => JSON.stringify([command, ...args]);
  const files = host.files ?? {};
  const commands = host.commands ?? {};
  const layer = Layer.mergeAll(
    FileSystem.layerNoop({
      exists: (path) => {
        const file = files[path];
        if (file !== undefined && file.error === "exists")
          return Effect.fail(new Error("exists failed"));
        return Effect.succeed(file !== undefined);
      },
      readFileString: (path) => {
        const file = files[path];
        if (file === undefined || file.error === "read")
          return Effect.fail(new Error("read failed"));
        return Effect.succeed(file.text);
      },
    }),
    Layer.succeed(
      ProcessRunner.ProcessRunner,
      ProcessRunner.ProcessRunner.of({
        run: (input) => {
          const answer = commands[key(input.command, input.args)];
          if (answer === undefined || answer.fails) return Effect.fail(new Error("spawn failed"));
          return Effect.succeed({
            stdout: answer.stdout,
            stderr: "",
            code: ChildProcessSpawner.ExitCode(answer.code),
          });
        },
      }),
    ),
    Layer.succeed(HostProcessPlatform, host.platform),
    Layer.succeed(HostProcessHostname, host.hostname ?? "host"),
  );
  return Effect.runPromise(effect.pipe(Effect.provide(layer)));
};

// ---- pure functions
const products = [
  "Mac mini (2024)",
  "Macmini8,1",
  "  Mac Studio ",
  "MacStudio1,2",
  "MacBook Pro",
  "MacBookAir10,1",
  "iMac24,1",
  "iMac",
  "Mac Pro",
  "MacPro7,1",
  "mac mini",
  "MAC  MINI",
  "Dell XPS",
  "",
  "Mac",
  "Macmin",
];
for (const product of products)
  rows.push({ kind: "apple", input: product, output: machineKindFromAppleProductName(product) });
const chassis = [
  null,
  "1",
  "2",
  "3",
  "4",
  "5",
  "6",
  "7",
  "8",
  "9",
  "10",
  "11",
  "12",
  "13",
  "14",
  "15",
  "16",
  "17",
  "18",
  "19",
  "20",
  "21",
  "22",
  "23",
  "24",
  "25",
  "27",
  "28",
  "29",
  "30",
  "31",
  "32",
  "33",
  "34",
  "35",
  "36",
  "x",
  "",
];
const vendors = [
  null,
  "Dell Inc.",
  "QEMU",
  "Microsoft Corporation",
  "Amazon EC2",
  "Google",
  "innotek GmbH",
  "Apple Inc.",
  "Hetzner",
  "Some cloud vendor",
  "LENOVO",
];
const names = [
  null,
  "",
  "Virtual Machine",
  "Surface Laptop 5",
  "Standard PC (Q35 + ICH9, 2009)",
  "VirtualBox",
  "Mac mini",
  "MacBookPro17,1",
  "Google Compute Engine",
  "OptiPlex 7090",
  "HVM domU",
  "KVM",
];
for (const chassisType of chassis) {
  for (const sysVendor of vendors) {
    for (const productName of names) {
      if ((sysVendor === null) !== (productName === null) && chassisType !== "3") continue;
      rows.push({
        kind: "dmi",
        input: { chassisType, sysVendor, productName },
        output: machineKindFromDmi({ chassisType, sysVendor, productName }),
      });
    }
  }
}

// ---- scripted hosts
const ok = (stdout, code = 0) => ({ stdout, code });
const cmd = (command, ...args) => JSON.stringify([command, ...args]);
const hosts = [
  { name: "windows", platform: "win32", hostname: "macbook-pro" },
  { name: "windows blank hostname", platform: "win32", hostname: "  ", cwdBaseName: "t3code" },
  { name: "windows nothing", platform: "win32", hostname: "", cwdBaseName: "  " },
  {
    name: "mac computer name",
    platform: "darwin",
    hostname: "h",
    commands: { [cmd("scutil", "--get", "ComputerName")]: ok(" Julius's MacBook Pro \n") },
  },
  {
    name: "mac computer name fails",
    platform: "darwin",
    hostname: "fallback-host",
    commands: { [cmd("scutil", "--get", "ComputerName")]: ok("", 1) },
  },
  {
    name: "mac computer name empty",
    platform: "darwin",
    hostname: "fallback-host",
    commands: { [cmd("scutil", "--get", "ComputerName")]: ok("  \n") },
  },
  { name: "mac scutil cannot run", platform: "darwin", hostname: "fallback-host", commands: {} },
  {
    name: "mac mini by ioreg",
    platform: "darwin",
    hostname: "h",
    commands: {
      [cmd("ioreg", "-rd1", "-n", "product")]: ok('  "product-name" = <"Mac mini (2024)">\n'),
    },
  },
  {
    name: "mac studio by ioreg",
    platform: "darwin",
    hostname: "h",
    commands: { [cmd("ioreg", "-rd1", "-n", "product")]: ok('"product-name"=<"Mac Studio">') },
  },
  {
    name: "macbook by ioreg",
    platform: "darwin",
    hostname: "h",
    commands: { [cmd("ioreg", "-rd1", "-n", "product")]: ok('"product-name" = <"MacBook Pro">') },
  },
  {
    name: "ioreg unknown product, model known",
    platform: "darwin",
    hostname: "h",
    commands: {
      [cmd("ioreg", "-rd1", "-n", "product")]: ok('"product-name" = <"Thing">'),
      [cmd("sysctl", "-n", "hw.model")]: ok("Macmini8,1\n"),
    },
  },
  {
    name: "ioreg fails, model known",
    platform: "darwin",
    hostname: "h",
    commands: {
      [cmd("ioreg", "-rd1", "-n", "product")]: ok("", 1),
      [cmd("sysctl", "-n", "hw.model")]: ok("iMac20,1"),
    },
  },
  {
    name: "mac with no signal",
    platform: "darwin",
    hostname: "h",
    commands: { [cmd("sysctl", "-n", "hw.model")]: ok("Unknown1,1") },
  },
  { name: "mac nothing runs", platform: "darwin", hostname: "h", commands: {} },
  ...[
    ["quoted", 'PRETTY_HOSTNAME="Build Agent 01"\nICON_NAME="computer-vm"\n'],
    ["single quoted", "PRETTY_HOSTNAME='Single Quoted'\n"],
    ["unquoted", "PRETTY_HOSTNAME=Plain Name\n"],
    ["spaced", "  PRETTY_HOSTNAME=  Spaced Out  \n"],
    ["comment first", "# PRETTY_HOSTNAME=Commented\nPRETTY_HOSTNAME=Real\n"],
    ["crlf", 'ICON_NAME=x\r\nPRETTY_HOSTNAME="Crlf Name"\r\n'],
    ["empty value", "PRETTY_HOSTNAME=\n"],
    ["empty quotes", 'PRETTY_HOSTNAME=""\n'],
    ["other key", "ICON_NAME=computer\n"],
    ["prefix key", "XPRETTY_HOSTNAME=No\n"],
    ["first wins", "PRETTY_HOSTNAME=One\nPRETTY_HOSTNAME=Two\n"],
    ["lone quote", 'PRETTY_HOSTNAME="Unclosed\n'],
    ["blank file", "  \n"],
  ].map(([name, text]) => ({
    name: `linux machine-info ${name}`,
    platform: "linux",
    hostname: "linux-host",
    files: { "/etc/machine-info": { text } },
    commands: { [cmd("hostnamectl", "--pretty")]: ok("Pretty From hostnamectl\n") },
  })),
  {
    name: "linux hostnamectl",
    platform: "linux",
    hostname: "linux-host",
    commands: { [cmd("hostnamectl", "--pretty")]: ok("  Pretty Box \n") },
  },
  {
    name: "linux hostnamectl empty",
    platform: "linux",
    hostname: "linux-host",
    commands: { [cmd("hostnamectl", "--pretty")]: ok("\n") },
  },
  {
    name: "linux hostnamectl fails",
    platform: "linux",
    hostname: "linux-host",
    commands: { [cmd("hostnamectl", "--pretty")]: ok("", 1) },
  },
  { name: "linux no probes", platform: "linux", hostname: "linux-host", commands: {} },
  {
    name: "linux exists fails",
    platform: "linux",
    hostname: "linux-host",
    files: { "/etc/machine-info": { error: "exists", text: "" } },
    commands: { [cmd("hostnamectl", "--pretty")]: ok("Via Command") },
  },
  {
    name: "linux read fails",
    platform: "linux",
    hostname: "linux-host",
    files: { "/etc/machine-info": { error: "read", text: "" } },
    commands: {},
  },
  ...[
    [
      "wsl 2",
      {
        "/proc/sys/kernel/osrelease": "5.15.153.1-microsoft-standard-WSL2\n",
        "/sys/class/dmi/id/sys_vendor": "Microsoft Corporation",
        "/sys/class/dmi/id/product_name": "Virtual Machine",
        "/sys/class/dmi/id/chassis_type": "3",
      },
    ],
    ["wsl 1", { "/proc/sys/kernel/osrelease": "4.4.0-19041-Microsoft" }],
    [
      "qemu",
      {
        "/proc/sys/kernel/osrelease": "6.1.0",
        "/sys/class/dmi/id/sys_vendor": "QEMU",
        "/sys/class/dmi/id/product_name": "Standard PC",
        "/sys/class/dmi/id/chassis_type": "1",
      },
    ],
    [
      "laptop",
      {
        "/proc/sys/kernel/osrelease": "6.1.0",
        "/sys/class/dmi/id/sys_vendor": "LENOVO",
        "/sys/class/dmi/id/product_name": "ThinkPad",
        "/sys/class/dmi/id/chassis_type": "10\n",
      },
    ],
    [
      "desktop",
      {
        "/sys/class/dmi/id/sys_vendor": "Dell Inc.",
        "/sys/class/dmi/id/product_name": "OptiPlex",
        "/sys/class/dmi/id/chassis_type": "3",
      },
    ],
    [
      "server",
      {
        "/sys/class/dmi/id/sys_vendor": "Dell Inc.",
        "/sys/class/dmi/id/product_name": "PowerEdge",
        "/sys/class/dmi/id/chassis_type": "23",
      },
    ],
    [
      "asahi",
      {
        "/sys/class/dmi/id/sys_vendor": "Apple Inc.",
        "/sys/class/dmi/id/product_name": "Mac mini (M1, 2020)",
        "/sys/class/dmi/id/chassis_type": "3",
      },
    ],
    [
      "unknown chassis",
      {
        "/sys/class/dmi/id/sys_vendor": "Dell Inc.",
        "/sys/class/dmi/id/product_name": "Thing",
        "/sys/class/dmi/id/chassis_type": "30",
      },
    ],
    ["chassis only", { "/sys/class/dmi/id/chassis_type": "9" }],
    [
      "blank files",
      { "/sys/class/dmi/id/chassis_type": "  \n", "/sys/class/dmi/id/sys_vendor": "\n" },
    ],
    ["nothing", {}],
  ].map(([name, files]) => ({
    name: `linux machine ${name}`,
    platform: "linux",
    hostname: "h",
    files: Object.fromEntries(Object.entries(files).map(([path, text]) => [path, { text }])),
    commands: {},
  })),
  { name: "freebsd", platform: "freebsd", hostname: "bsd" },
];
for (const host of hosts) {
  const cwdBaseName = host.cwdBaseName ?? "project";
  rows.push({
    kind: "host",
    host: { ...host, cwdBaseName },
    label: await run(resolveServerEnvironmentLabel({ cwdBaseName }), host),
    machine: await run(detectServerEnvironmentMachineKind(), host),
  });
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/environment-host.jsonl", import.meta.url),
  `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`,
);
console.log(`${rows.length} rows`);
