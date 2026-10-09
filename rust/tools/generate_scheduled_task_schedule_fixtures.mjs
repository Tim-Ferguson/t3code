// Runs the original schedule arithmetic (apps/server/src/scheduledTasks/Schedule.ts) on the real
// Effect DateTime, once per time zone, and records what it produces. Fixed-time schedules read the
// environment's local zone, so each zone is a child process started with TZ set (V8 reads TZ at
// start). The grid of instants is built from the zone's own offset transitions, so every daylight
// saving gap and overlap in 2024-2027 is bracketed minute by minute, plus month-end, leap-day and
// year-end edges. Also records the webhook helpers that are pure: prompt templates, redaction and
// HMAC verification.
//   node tools/generate_scheduled_task_schedule_fixtures.mjs
import { spawnSync } from "node:child_process";
import { createHmac } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import * as NodePath from "node:path";
import { fileURLToPath } from "node:url";

const here = NodePath.dirname(fileURLToPath(import.meta.url));
const output = NodePath.join(here, "../crates/server/tests/fixtures/scheduled-task-schedule.jsonl");

const ZONES = [
  "UTC",
  "America/New_York",
  "Europe/London",
  "America/Los_Angeles",
  "Australia/Lord_Howe",
  "Asia/Kolkata",
  "Pacific/Auckland",
  "Europe/Dublin",
  "America/Sao_Paulo",
  "Asia/Kathmandu",
  "Australia/Sydney",
  "America/St_Johns",
  "Europe/Berlin",
];

const SCHEDULES = [
  { type: "fixed_time", timeOfDay: "00:00" },
  { type: "fixed_time", timeOfDay: "00:30" },
  { type: "fixed_time", timeOfDay: "01:30" },
  { type: "fixed_time", timeOfDay: "02:00" },
  { type: "fixed_time", timeOfDay: "02:30" },
  { type: "fixed_time", timeOfDay: "03:00" },
  { type: "fixed_time", timeOfDay: "9:05" },
  { type: "fixed_time", timeOfDay: "09:00", weekdays: [1, 2, 3, 4, 5] },
  { type: "fixed_time", timeOfDay: "23:30" },
  { type: "fixed_time", timeOfDay: "12:00", weekdays: [0, 6] },
  { type: "fixed_time", timeOfDay: "01:30", weekdays: [0] },
  { type: "fixed_time", timeOfDay: "10:00", weekdays: [] },
  { type: "fixed_time", timeOfDay: "10:00", weekdays: [3, 3] },
  { type: "fixed_time", timeOfDay: "08:15", weekdays: [0, 1, 2, 3, 4, 5, 6] },
  { type: "interval", everyMs: 60_000 },
  { type: "interval", everyMs: 3_600_000 },
  { type: "interval", everyMs: 5_400_000 },
  { type: "interval", everyMs: 86_400_000 },
  { type: "interval", everyMs: 1_000 },
  { type: "interval", everyMs: 90_061_001 },
  { type: "webhook", signature: null },
];

// ---- worker: runs under one TZ -----------------------------------------------------------------
if (process.argv[2] === "--worker") {
  const DateTime = await import("../../apps/server/node_modules/effect/dist/DateTime.js");
  const { nextScheduledRunAt, isSameSchedule, isMissedFixedTimeRun, parseTimeOfDay } =
    await import("../../apps/server/src/scheduledTasks/Schedule.ts");
  const input = JSON.parse(
    await new Promise((resolve) => {
      let data = "";
      process.stdin.on("data", (chunk) => (data += chunk));
      process.stdin.on("end", () => resolve(data));
    }),
  );
  const zone = DateTime.zoneMakeLocal();
  const local = (ms) => DateTime.setZone(DateTime.makeUnsafe(ms), zone);
  const iso = (value) => {
    try {
      if (value === null || !Number.isFinite(DateTime.toEpochMillis(value))) return null;
      return DateTime.formatIso(DateTime.toUtc(value));
    } catch {
      return null;
    }
  };
  const rows = [];
  for (const { schedule, from } of input.next) {
    let next = null;
    try {
      next = iso(nextScheduledRunAt(schedule, local(from)));
    } catch {
      next = null;
    }
    rows.push({ schedule, from, next });
  }
  const sameness = input.same.map(([a, b]) => ({ a, b, same: isSameSchedule(a, b) }));
  const missed = input.missed.map(({ schedule, dueAt, now }) => ({
    schedule,
    dueAt,
    now,
    missed: isMissedFixedTimeRun(schedule, DateTime.makeUnsafe(dueAt), DateTime.makeUnsafe(now)),
  }));
  const times = input.times.map((value) => ({ value, parsed: parseTimeOfDay(value) }));
  // Through a file: a pipe may still hold unread output when the process ends.
  writeFileSync(
    process.argv[3],
    JSON.stringify({
      zone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      rows,
      sameness,
      missed,
      times,
    }),
  );
} else {
  // ---- driver ------------------------------------------------------------------------------------
  const MINUTE = 60_000;
  const HOUR = 60 * MINUTE;
  const DAY = 24 * HOUR;

  /** Offset transitions of a zone in [from, to), found by scanning hourly and bisecting to the minute. */
  function transitions(zone, from, to) {
    const format = new Intl.DateTimeFormat("en-US", { timeZone: zone, timeZoneName: "longOffset" });
    const offset = (ms) =>
      format.formatToParts(ms).find((part) => part.type === "timeZoneName").value;
    const found = [];
    let previous = offset(from);
    for (let at = from + HOUR; at < to; at += HOUR) {
      const current = offset(at);
      if (current !== previous) {
        let low = at - HOUR;
        let high = at;
        while (high - low > MINUTE) {
          const middle = low + Math.floor((high - low) / 2 / MINUTE) * MINUTE;
          if (offset(middle) === previous) low = middle;
          else high = middle;
        }
        found.push(high);
        previous = current;
      }
    }
    return found;
  }

  // A deterministic generator, so the fixture file only changes when the original does.
  let seed = 0x2f6e2b1;
  const random = () => {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    return seed / 0x100000000;
  };

  const start = Date.UTC(2024, 0, 1);
  const end = Date.UTC(2028, 0, 1);
  const genericInstants = [
    Date.UTC(2026, 6, 1, 16, 0),
    Date.UTC(2026, 6, 3, 17, 0),
    Date.UTC(2026, 0, 31, 23, 59, 30),
    Date.UTC(2026, 1, 28, 23, 0),
    Date.UTC(2028, 1, 29, 12, 0),
    Date.UTC(2028, 1, 28, 23, 59, 59, 999),
    Date.UTC(2024, 1, 29, 8, 30),
    Date.UTC(2025, 11, 31, 23, 30),
    Date.UTC(2026, 11, 31, 22, 0),
    Date.UTC(2026, 3, 30, 22, 30),
    Date.UTC(2026, 9, 31, 23, 59),
    Date.UTC(2026, 5, 30, 23, 59, 59, 500),
    Date.UTC(2026, 8, 9, 12, 0, 0, 0),
    Date.UTC(2026, 8, 9, 9, 0, 0, 0),
    Date.UTC(2027, 2, 31, 12, 0),
    Date.UTC(1999, 11, 31, 23, 59),
    Date.UTC(2100, 0, 1, 0, 0),
    Date.UTC(2038, 0, 19, 3, 14, 8),
  ];
  for (let index = 0; index < 14; index += 1)
    genericInstants.push(start + Math.floor(random() * (end - start)));

  const nextCases = {};
  const sameCases = [];
  const missedCases = [];
  const times = [
    "09:30",
    "23:59",
    "25:00",
    "00:00",
    "0:00",
    "9:5",
    "24:00",
    "09:60",
    " 09:30 ",
    "09:30:00",
    "",
    "ab:cd",
    "1:30",
    "12:3",
    "-1:30",
    "09 :30",
    "009:30",
    "+9:30",
    "٠٩:٣٠",
    "09:30\n",
  ];

  const offsets = [
    -2 * DAY,
    -1 * DAY,
    -3 * HOUR,
    -2 * HOUR,
    -HOUR - 30 * MINUTE,
    -HOUR,
    -45 * MINUTE,
    -30 * MINUTE,
    -MINUTE,
    0,
    MINUTE,
    29 * MINUTE,
    30 * MINUTE,
    59 * MINUTE,
    HOUR,
    HOUR + 30 * MINUTE,
    2 * HOUR,
    3 * HOUR,
    6 * HOUR,
    DAY,
    2 * DAY,
  ];
  for (const zone of ZONES) {
    const instants = new Set(genericInstants);
    for (const at of transitions(zone, start, end))
      for (const offset of offsets) instants.add(at + offset);
    // Second-and-millisecond precision matters at a boundary: a from equal to the slot is not "after" it.
    nextCases[zone] = [];
    for (const from of [...instants].toSorted((a, b) => a - b)) {
      for (const schedule of SCHEDULES) {
        // Every instant meets a seeded third of the schedules, which keeps the file small and still
        // crosses each schedule with each transition many times.
        if (random() < 0.7) continue;
        nextCases[zone].push({ schedule, from });
      }
    }
    // Slots exactly on a minute boundary, so "after from" excludes an equal instant.
    for (const [hour, minute] of [
      [9, 0],
      [2, 30],
      [1, 30],
      [0, 0],
    ]) {
      for (const probe of [
        Date.UTC(2026, 2, 8, 10 + hour, minute),
        Date.UTC(2026, 10, 1, 4 + hour, minute),
      ]) {
        for (const delta of [-1, 0, 1])
          nextCases[zone].push({
            schedule: {
              type: "fixed_time",
              timeOfDay: `${hour}:${String(minute).padStart(2, "0")}`,
            },
            from: probe + delta,
          });
      }
    }
    // The legacy and absurd intervals, whose arithmetic overflows the representable date range.
    for (const everyMs of [
      8_640_000_000_000_000, 9_000_000_000_000_000, 253_402_300_800_000, 300_000_000_000_000, 1e15,
    ]) {
      nextCases[zone].push({
        schedule: { type: "interval", everyMs },
        from: Date.UTC(2026, 8, 9, 12, 0),
      });
    }
  }
  const equalForms = [
    [
      { type: "interval", everyMs: 60_000 },
      { type: "interval", everyMs: 60_000 },
    ],
    [
      { type: "interval", everyMs: 60_000 },
      { type: "interval", everyMs: 120_000 },
    ],
    [
      { type: "interval", everyMs: 60_000 },
      { type: "fixed_time", timeOfDay: "09:00" },
    ],
    [
      { type: "fixed_time", timeOfDay: "9:00" },
      { type: "fixed_time", timeOfDay: "09:00" },
    ],
    [
      { type: "fixed_time", timeOfDay: "9:00" },
      { type: "fixed_time", timeOfDay: "09:01" },
    ],
    [
      { type: "fixed_time", timeOfDay: "09:00" },
      { type: "interval", everyMs: 60_000 },
    ],
    [
      { type: "fixed_time", timeOfDay: "09:00", weekdays: [] },
      { type: "fixed_time", timeOfDay: "09:00" },
    ],
    [
      { type: "fixed_time", timeOfDay: "09:00", weekdays: [0, 1, 2, 3, 4, 5, 6] },
      { type: "fixed_time", timeOfDay: "09:00" },
    ],
    [
      { type: "fixed_time", timeOfDay: "09:00", weekdays: [1, 2] },
      { type: "fixed_time", timeOfDay: "09:00", weekdays: [2, 1, 2] },
    ],
    [
      { type: "fixed_time", timeOfDay: "09:00", weekdays: [1, 2] },
      { type: "fixed_time", timeOfDay: "09:00", weekdays: [1, 3] },
    ],
    [
      { type: "fixed_time", timeOfDay: "09:00", weekdays: [1] },
      { type: "fixed_time", timeOfDay: "09:00" },
    ],
    [
      { type: "fixed_time", timeOfDay: "25:00" },
      { type: "fixed_time", timeOfDay: "25:00" },
    ],
    [
      { type: "fixed_time", timeOfDay: "09:00" },
      { type: "fixed_time", timeOfDay: "25:00" },
    ],
    [
      { type: "webhook", signature: null },
      { type: "webhook", signature: { header: "x", encoding: "hex", prefix: "" } },
    ],
    [
      { type: "webhook", signature: null },
      { type: "interval", everyMs: 60_000 },
    ],
    [
      { type: "interval", everyMs: 60_000 },
      { type: "webhook", signature: null },
    ],
    [
      { type: "webhook", signature: null },
      { type: "fixed_time", timeOfDay: "09:00" },
    ],
    [
      { type: "fixed_time", timeOfDay: "09:00" },
      { type: "webhook", signature: null },
    ],
  ];
  sameCases.push(...equalForms);
  const dueAt = Date.UTC(2026, 6, 1, 9, 0);
  for (const schedule of [
    { type: "fixed_time", timeOfDay: "09:00" },
    { type: "interval", everyMs: 60_000 },
    { type: "webhook", signature: null },
  ]) {
    for (const late of [
      -MINUTE,
      0,
      5 * MINUTE,
      10 * MINUTE,
      10 * MINUTE + 1,
      10 * MINUTE + 999,
      11 * MINUTE,
      6 * HOUR,
      DAY,
    ]) {
      missedCases.push({ schedule, dueAt, now: dueAt + late });
    }
  }

  const rows = [];
  rows.push({ type: "constants", minIntervalMs: 60_000, missedFixedTimeGraceMs: 10 * MINUTE });
  for (const zone of ZONES) {
    const scratch = mkdtempSync(NodePath.join(tmpdir(), "t3-schedule-fixtures-"));
    const result = spawnSync(
      process.execPath,
      [
        "--no-warnings",
        fileURLToPath(import.meta.url),
        "--worker",
        NodePath.join(scratch, "out.json"),
      ],
      {
        env: { ...process.env, TZ: zone },
        input: JSON.stringify({
          next: nextCases[zone],
          same: zone === ZONES[0] ? sameCases : [],
          missed: zone === ZONES[0] ? missedCases : [],
          times: zone === ZONES[0] ? times : [],
        }),
        encoding: "utf8",
        maxBuffer: 256 * 1024 * 1024,
      },
    );
    if (result.status !== 0) throw new Error(`worker for ${zone} failed: ${result.stderr}`);
    const parsed = JSON.parse(readFileSync(NodePath.join(scratch, "out.json"), "utf8"));
    rmSync(scratch, { recursive: true, force: true });
    // ICU may report an alias (Asia/Calcutta for Asia/Kolkata); the offset at a summer and a winter instant must agree.
    const offsetAt = (id, at) =>
      new Intl.DateTimeFormat("en-US", { timeZone: id, timeZoneName: "longOffset" })
        .formatToParts(at)
        .find((part) => part.type === "timeZoneName").value;
    for (const at of [Date.UTC(2026, 0, 15), Date.UTC(2026, 6, 15)])
      if (offsetAt(parsed.zone, at) !== offsetAt(zone, at))
        throw new Error(`worker reported zone ${parsed.zone}, wanted ${zone}`);
    for (const row of parsed.rows) rows.push({ type: "next", zone, ...row });
    for (const row of parsed.sameness) rows.push({ type: "same", ...row });
    for (const row of parsed.missed) rows.push({ type: "missed", ...row });
    for (const row of parsed.times) rows.push({ type: "time", ...row });
  }

  // ---- webhook helpers ---------------------------------------------------------------------------
  const Template = await import("../../apps/server/src/scheduledTasks/webhookTemplate.ts");
  const Verification = await import("../../apps/server/src/scheduledTasks/webhookVerification.ts");
  const request = (overrides = {}) => ({
    method: "POST",
    path: "/api/hooks/task",
    query: "",
    headers: {},
    bodyText: "",
    ...overrides,
  });
  const json = JSON.stringify;
  const templateCases = [
    [
      "Review {{body.pull_request.html_url}}",
      request({
        headers: { "content-type": "application/json" },
        bodyText: json({ pull_request: { html_url: "https://example.test/pull/1" } }),
      }),
    ],
    ["{{ body.a.b.0 }} and {{body.a.b.1}}", request({ bodyText: json({ a: { b: ["x", 2] } }) })],
    ["{{body.missing}}|{{body.a.zz}}|{{body.missing}}", request({ bodyText: json({ a: 1 }) })],
    ["{{body.a}}", request({ bodyText: json({ a: { nested: [1, 2, { deep: true }] } }) })],
    [
      "{{body.flag}} {{body.count}} {{body.nothing}}",
      request({ bodyText: json({ flag: false, count: 0, nothing: null }) }),
    ],
    ["{{body}}", request({ bodyText: "raw text body" })],
    [
      "{{body.x}}",
      request({
        headers: { "content-type": "application/x-www-form-urlencoded" },
        bodyText: "x=1&y=two+words&x=3&z=%E2%9C%93",
      }),
    ],
    [
      "{{body.y}}",
      request({
        headers: { "content-type": "application/x-www-form-urlencoded; charset=utf-8" },
        bodyText: "x=1&y=two+words",
      }),
    ],
    ["{{headers.X-Event}} {{headers.x-event}}", request({ headers: { "x-event": "push" } })],
    [
      "{{headers.authorization}}|{{headers}}",
      request({
        headers: {
          authorization: "Bearer abc",
          "x-event": "push",
          "x-api-key": "k",
          cookie: "a=b",
        },
      }),
    ],
    [
      "{{query.name}} {{query.other}} {{query.token}}",
      request({ query: "name=a%20b&token=hush&name=second" }),
    ],
    ["{{query}}", request({ query: "name=a&access_token=hush&api_key=zzz&plain=1&flag" })],
    [
      "{{request}}",
      request({
        query: "a=1&secret=2",
        headers: { "x-event": "push", authorization: "Bearer abc", "content-type": "text/plain" },
        bodyText: "hello\nworld",
      }),
    ],
    [
      "{{request}}",
      request({ method: "GET", path: "/api/hooks/id", query: "", headers: {}, bodyText: "" }),
    ],
    [
      "{{request.method}} {{unknown}} {{}} {{ }} {{headers.}} {{body.}}",
      request({ bodyText: json({ "": "empty" }) }),
    ],
    [
      "{{body.__proto__}} {{body.constructor}} {{body.toString}}",
      request({ bodyText: json({ a: 1 }) }),
    ],
    ["{{body.length}} {{body.0}}", request({ bodyText: json([1, 2, 3]) })],
    ["{{body.0.name}}", request({ bodyText: json([{ name: "first" }]) })],
    ["no placeholders at all", request()],
    ["{{{body.a}}} {body.a} {{body.a}", request({ bodyText: json({ a: "v" }) })],
    ["{{body.a}}{{body.a}}", request({ bodyText: json({ a: "twice" }) })],
    ["{{body.emoji}} {{body.日本}}", request({ bodyText: json({ emoji: "😀", 日本: "語" }) })],
    [
      "{{body.big}} {{body.neg}} {{body.frac}} {{body.exp}}",
      request({ bodyText: '{"big":12345678901234567890,"neg":-0,"frac":1.50,"exp":1e21}' }),
    ],
    [
      "{{headers.set-cookie}}|{{headers.x-token}}|{{headers.proxy-authorization}}",
      request({ headers: { "set-cookie": "s=1", "x-token": "t", "proxy-authorization": "p" } }),
    ],
    ["{{body.a.b}}", request({ bodyText: json({ a: "string" }) })],
    ["{{body.a}}", request({ bodyText: "   " })],
    ["{{body.a}}", request({ bodyText: "{broken" })],
    ["{{query.a.b}}", request({ query: "a.b=dotted" })],
    ["{{headers.x.y}}", request({ headers: { "x.y": "dotted" } })],
  ];
  for (const [template, req] of templateCases)
    rows.push({
      type: "template",
      template,
      request: req,
      rendered: json_(Template.renderWebhookPrompt(template, req)),
    });
  function json_(value) {
    return JSON.parse(JSON.stringify(value));
  }
  for (const headers of [
    {},
    {
      authorization: "a",
      cookie: "c",
      "x-hub-signature-256": "sig",
      "x-api-key": "k",
      "x-plain": "p",
      "x-Auth-Token": "t",
      password: "p",
      "x-keyboard": "kb",
      monkey: "m",
    },
  ]) {
    rows.push({ type: "redact-headers", headers, redacted: Template.redactHeaders(headers) });
  }
  for (const query of [
    "",
    "a=1",
    "token=abc&x=1",
    "x=1&TOKEN=abc",
    "pass%77ord=1&ok=2",
    "bad%zz=1&secret=2",
    "flag&key",
    "api_key=a=b&y",
    "a=1&&b=2",
    "%74oken=encoded",
    "au%74h=1",
    "n+ame=1&secret+key=2",
    "x=%E2%9C%93&sig=1",
  ]) {
    rows.push({ type: "redact-query", query, redacted: Template.redactQuery(query) });
  }
  const body = Buffer.from("payload-é-bytes");
  const secret = "s3cr3t";
  const hex = createHmac("sha256", secret).update(body).digest("hex");
  const b64 = createHmac("sha256", secret).update(body).digest("base64");
  const verificationCases = [
    [
      { header: "X-Hub-Signature-256", encoding: "hex", prefix: "sha256=" },
      secret,
      { "x-hub-signature-256": `sha256=${hex}` },
    ],
    [
      { header: "X-Hub-Signature-256", encoding: "hex", prefix: "sha256=" },
      secret,
      { "x-hub-signature-256": `SHA256=${hex.toUpperCase()}` },
    ],
    [
      { header: "X-Hub-Signature-256", encoding: "hex", prefix: "sha256=" },
      secret,
      { "x-hub-signature-256": `  sha256=${hex}  ` },
    ],
    [
      { header: "x-hub-signature-256", encoding: "hex", prefix: "sha256=" },
      secret,
      { "x-hub-signature-256": `sha256=${hex.replace(/.$/, "0")}` },
    ],
    [
      { header: "x-hub-signature-256", encoding: "hex", prefix: "sha256=" },
      secret,
      { "x-hub-signature-256": hex },
    ],
    [{ header: "x-hub-signature-256", encoding: "hex", prefix: "sha256=" }, secret, {}],
    [
      { header: "x-hub-signature-256", encoding: "hex", prefix: "sha256=" },
      "other",
      { "x-hub-signature-256": `sha256=${hex}` },
    ],
    [{ header: "x-sig", encoding: "base64", prefix: "" }, secret, { "x-sig": b64 }],
    [{ header: "x-sig", encoding: "base64", prefix: "" }, secret, { "x-sig": b64.toLowerCase() }],
    [{ header: "x-sig", encoding: "base64", prefix: "v1=" }, secret, { "x-sig": `V1=${b64}` }],
    [{ header: "x-sig", encoding: "hex", prefix: "" }, secret, { "x-sig": hex }],
    [{ header: "x-sig", encoding: "hex", prefix: "" }, secret, { "x-sig": hex.toUpperCase() }],
    [{ header: "x-sig", encoding: "hex", prefix: "" }, secret, { "x-sig": `${hex}00` }],
    [{ header: "x-sig", encoding: "hex", prefix: "" }, secret, { "x-sig": "" }],
    [
      { header: "x-sig", encoding: "hex", prefix: "" },
      "",
      { "x-sig": createHmac("sha256", "").update(body).digest("hex") },
    ],
  ];
  for (const [signature, key, headers] of verificationCases) {
    rows.push({
      type: "verify",
      signature,
      secret: key,
      headers,
      bodyBase64: body.toString("base64"),
      verified: Verification.verifyWebhookSignature({
        signature,
        secret: key,
        headers,
        body: new Uint8Array(body),
      }),
    });
  }
  for (const [a, b] of [
    ["a", "a"],
    ["a", "b"],
    ["", ""],
    ["token", "token "],
    ["é", "é"],
    ["abc", "abcd"],
  ])
    rows.push({ type: "equals", a, b, equal: Verification.constantTimeEquals(a, b) });

  mkdirSync(NodePath.dirname(output), { recursive: true });
  const temporary = `${output}.tmp`;
  writeFileSync(temporary, `${rows.map((row) => JSON.stringify(row)).join("\n")}\n`);
  renameSync(temporary, output);
  console.log(`${rows.length} rows -> ${output}`);
}
