// Actual PortScanner pure functions, with the original shared host vocabulary.
import { readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { isLoopbackHost, LSOF_LOCAL_HOST_TOKENS } from "../../packages/shared/src/preview.ts";
const source = readFileSync(
  new URL("../../apps/server/src/preview/PortScanner.ts", import.meta.url),
  "utf8",
);
const code = source.slice(
  source.indexOf("const terminalOwnerKey"),
  source.indexOf("/** @public Service construction"),
);
const original = new Function(
  "isLoopbackHost",
  "LSOF_LOCAL_HOST_TOKENS",
  "CONFIGURED_LOCAL_SERVER_URLS_MAX_ITEMS",
  "PREVIEW_URL_MAX_LENGTH",
  stripTypeScriptTypes(code, { mode: "strip" }) +
    "\nreturn {parseLsofOutput,parsePortFromLsofName,parseWindowsListenerOutput,normalizeConfiguredUrls,webProbeCacheKey};",
)(isLoopbackHost, LSOF_LOCAL_HOST_TOKENS, 32, 2048);
const cases = [];
const owners = [
  [12, { threadId: "thread-one", terminalId: "term-one" }],
  [13, { threadId: "thread-two", terminalId: "term-two" }],
];
for (const host of [
  "*",
  "localhost",
  "127.0.0.1",
  "0.0.0.0",
  "::1",
  "[::1]",
  "[::]",
  "::",
  "LOCALHOST",
  "192.168.1.10",
  "",
])
  for (const port of [
    "5173",
    "0",
    "65535",
    "65536",
    "-1",
    "+13",
    " 21",
    "12junk",
    "0x20",
    "12.5",
    "1e3",
    "",
  ]) {
    const name = `${host}:${port}`;
    for (const suffix of ["", " (LISTEN)", "\r"])
      cases.push({
        kind: "port",
        input: name + suffix,
        expected: original.parsePortFromLsofName(name + suffix),
      });
    cases.push({
      kind: "windows",
      owners,
      input: `${host}|${port}|12| node |ignored`,
      expected: original.parseWindowsListenerOutput(
        `${host}|${port}|12| node |ignored`,
        new Map(owners),
      ),
    });
  }
for (const pid of [
  "12",
  "13",
  "0",
  "-1",
  "+12junk",
  "1e3",
  "0x12",
  "9007199254740992",
  "999999999999999999999999999999999999",
  "",
]) {
  const input = `p${pid}\nc\ufeffnode\ufeff\nn*:5173\np13\ncpython\nnlocalhost:5173\nn[::1]:3000\np12\nn*:6000\nn192.168.1.1:7000\n`;
  cases.push({
    kind: "lsof",
    owners,
    input,
    expected: original.parseLsofOutput(input, new Map(owners)),
  });
}
for (const pid of [
  "",
  "12",
  "12.0",
  "1e1",
  "0x0c",
  "-1",
  "Infinity",
  "NaN",
  "inf",
  "+0x0c",
  "999999999999999999999999999999",
]) {
  const input = `127.0.0.1|5173|${pid}| node \r\n::|3000|13|\ufeff python \ufeff\n127.0.0.1|5173|13|duplicate\n`;
  cases.push({
    kind: "windows",
    owners,
    input,
    expected: original.parseWindowsListenerOutput(input, new Map(owners)),
  });
}
const urls = [
  "http://localhost:3000",
  "https://localhost",
  "http://127.0.0.1/a?q=1#one",
  "http://[::1]:3000/a",
  "http://0.0.0.0:3000/a",
  "http://LOCALHOST:80",
  " http://localhost/ ",
  "http://localhost/😀",
  "ftp://localhost",
  "http://192.168.1.1",
  "http://localhost:0",
  "http://user:pass@localhost/path#two",
  "http://localhost/" + "a".repeat(2048),
  "http://localhost/" + "😀".repeat(1020),
  "not a url",
];
for (const input of [
  urls,
  Array(32).fill("invalid").concat("http://localhost:3000"),
  urls.toReversed(),
  ["http://0.0.0.0", "http://localhost/", "http://localhost/#x", "http://localhost/#y"],
])
  cases.push({ kind: "urls", input, expected: original.normalizeConfiguredUrls(input) });
for (const input of urls) {
  try {
    cases.push({ kind: "cache", input, expected: original.webProbeCacheKey(input) });
  } catch {}
}
writeFileSync(
  new URL("../crates/server/tests/fixtures/resource-ports.jsonl", import.meta.url),
  cases.map((c) => JSON.stringify(c)).join("\n") + "\n",
);
console.log(`${cases.length} original port/URL parsing witnesses`);
