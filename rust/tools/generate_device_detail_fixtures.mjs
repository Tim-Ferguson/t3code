// Development oracle: unchanged DeviceActions reads with injected command results.
import { writeFileSync, renameSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url));
const actions = await import(pathToFileURL(root + "apps/server/src/device/DeviceActions.ts"));
const Effect = await import(
  pathToFileURL(root + "packages/contracts/node_modules/effect/dist/Effect.js")
);
const rows = [];
const ios = [
  "light",
  "large",
  "enabled",
  '{"reduce-motion":"on","reduce-transparency":"off","show-borders":"on","voiceover":"off","liquid-glass":"clear","color-filter":"none"}',
];
const android = [
  "Night mode: yes",
  "1.0",
  "1",
  "1",
  "mCurrentFocus=Window{abc u0 fixture.app/Activity}",
];
const variants = {
  ios: [
    ["dark", "LIGHT", "\uFEFFdark\uFEFF", "light\u0085", "", "unknown", null],
    [
      "small",
      "extra-small",
      "medium",
      "large",
      "extra-extra-large",
      "extra-large",
      "accessibility-small",
      "accessibility-large",
      "unknown",
      "Extra-extra-large",
      "",
      null,
    ],
    ["disabled", "ENABLED", "unknown", "\uFEFFenabled\uFEFF", "", null],
    [
      "{}",
      "[]",
      "null",
      "false",
      '{"reduce-motion":"on","extra":1}',
      '{"reduce-motion":"ON"}',
      '{"reduce-motion":"off","color-filter":"red-green","liquid-glass":"tinted"}',
      '{"voiceover":"on","color-filter":"green-red"}',
      '{"show-borders":"off","color-filter":"blue-yellow"}',
      '{"color-filter":"grayscale"}',
      '{"color-filter":"invalid"}',
      "bad json",
      null,
    ],
  ],
  android: [
    ["yes no", "no yes", "no", "YES", "yesterday", "unknown", "", null],
    [
      "0.9",
      "0.90001",
      "1.0999",
      "1.1",
      "1.2499",
      "1.25",
      "-0",
      "0x1",
      "0b1",
      "0o1",
      "-0x1",
      "Infinity",
      "-Infinity",
      "NaN",
      "inf",
      "1_0",
      "1e-1",
      "1e309",
      "1.25\uFEFF",
      "1.25\u0085",
      "null",
      "",
      null,
    ],
    [
      "0",
      "-0",
      "",
      "null",
      "NaN",
      "Infinity",
      "0x0",
      "0b0",
      "+0",
      "0.0",
      " \uFEFF ",
      "invalid",
      "1",
      null,
    ],
    ["0", "1", "true", "01", "", "null", null],
    [
      "mFocusedApp=AppWindowToken{abc u123 other.app/Activity}",
      "mCurrentFocus=窗{abc u0 rejected.app/Main}",
      "mCurrentFocus=Window{abc u１２ rejected.app/Main}",
      "mCurrentFocus=Window{abc u0 app-with-hyphen/Main}",
      "mCurrentFocus=Window{abc u0 first.app/Main}\nmFocusedApp=AppWindowToken{abc u1 second.app/Main}",
      "unknown",
      "",
      null,
    ],
  ],
};
for (const platform of ["ios", "android"]) {
  const baseline = platform === "ios" ? ios : android;
  const combinations = [
    baseline,
    baseline.map(() => null),
    ...variants[platform].flatMap((options, index) =>
      options.map((value) => baseline.map((entry, i) => (i === index ? value : entry))),
    ),
  ];
  for (const outputs of combinations)
    for (const helper of platform === "ios" ? ["/fixture/ax", null] : [null]) {
      let index = 0;
      const calls = [];
      const ready = {
        helpers: { serveSimAxSettings: helper },
        run: (command, args) => {
          const output = outputs[index++];
          calls.push({ command, args });
          return output === null
            ? Effect.fail(new Error("fixture command failed"))
            : Effect.succeed({ code: 0, stdout: output, stderr: "" });
        },
      };
      const result = await Effect.runPromise(
        actions.readDeviceDetail(ready, platform, "fixture-id"),
      );
      rows.push({ platform, helper, outputs, calls, result });
    }
}
const path = root + "rust/crates/server/tests/fixtures/device-detail.jsonl";
writeFileSync(path + ".tmp", rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(path + ".tmp", path);
console.log(JSON.stringify({ cases: rows.length }));
