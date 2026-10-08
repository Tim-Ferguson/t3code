// Development-only oracle exercises the unchanged authenticated proxy layer.
import { writeFileSync, renameSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url));
const Effect = await import(
  pathToFileURL(root + "packages/contracts/node_modules/effect/dist/Effect.js")
);
const Layer = await import(
  pathToFileURL(root + "packages/contracts/node_modules/effect/dist/Layer.js")
);
const { HttpClient, HttpClientResponse, HttpRouter } = await import(
  pathToFileURL(root + "packages/contracts/node_modules/effect/dist/http/index.js")
);
const auth = await import(pathToFileURL(root + "apps/server/src/auth/EnvironmentAuth.ts"));
const devices = await import(pathToFileURL(root + "apps/server/src/device/DeviceService.ts"));
const proxy = await import(pathToFileURL(root + "apps/server/src/device/DeviceHubProxy.ts"));
let scopes = [];
let forwarded = [];
const client = HttpClient.make((request) => {
  forwarded.push({
    url: request.url,
    headers: Object.fromEntries(
      Object.entries(request.headers).filter(([key]) => key !== "b3" && key !== "traceparent"),
    ),
    method: request.method,
  });
  return Effect.succeed(HttpClientResponse.fromWeb(request, new Response("fixture")));
});
const { handler, dispose } = HttpRouter.toWebHandler(
  proxy.layer.pipe(
    Layer.provideMerge(
      Layer.succeed(auth.EnvironmentAuth, {
        authenticateWebSocketUpgrade: () =>
          Effect.succeed({
            sessionId: "fixture",
            subject: "fixture",
            method: "bearer-access-token",
            scopes,
          }),
      }),
    ),
    Layer.provideMerge(
      Layer.succeed(devices.DeviceService, {
        currentReadiness: () =>
          Effect.succeed({ hostId: "local", hub: { origin: "http://hub.test" } }),
      }),
    ),
    Layer.provideMerge(Layer.succeed(HttpClient.HttpClient, client)),
  ),
  { disableLogger: true },
);
const paths = [
  "/api/devices",
  "/api/devices/ws",
  "/vendor/serve-sim/api",
  "/vendor/serve-sim/api/screenshot",
  "/vendor/serve-sim/api/event-log",
  "/vendor/serve-sim/api/event-log/events",
  "/vendor/serve-sim/helper/fixture/stream.mjpeg",
  "/vendor/serve-sim/helper/fixture/stream.avcc",
  "/vendor/serve-sim/helper/fixture/config",
  "/vendor/serve-sim/helper/fixture/health",
  "/vendor/serve-sim/helper/fixture/ax",
  "/vendor/serve-sim/helper/fixture/foreground",
  "/vendor/serve-sim/helper/duo/panel/1/stream.avcc",
  "/vendor/serve-sim/helper/duo/panel/3/stream.avcc",
  "/vendor/serve-sim/helper/duo/panel/2/stream.avcc",
  "/vendor/serve-sim/helper/duo/panel/1/webrtc/offer",
  "/vendor/serve-sim/helper/duo/panel/3/exec",
  "/vendor/serve-sim/helper/ws",
  "/vendor/serve-sim/appstate",
  "/vendor/serve-emu/api/devices",
  "/vendor/serve-emu/api/screenshot",
  "/vendor/serve-emu/api/stream-mode",
  "/vendor/serve-emu/api/stream-settings",
  "/vendor/serve-emu/api/accessibility",
  "/vendor/serve-emu/api/fold",
  "/vendor/serve-emu/health",
  "/vendor/serve-emu/ws",
  "/vendor/serve-sim/exec",
  "/vendor/serve-sim/api/shell",
  "/api/devices/boot",
  "/",
  "/vendor/serve-sim/helper//health",
  "/vendor/serve-sim/helper/fixture/health/",
];
const rows = [];
try {
  for (const path of paths)
    for (const method of ["GET", "HEAD", "POST", "PUT", "DELETE"])
      for (const upgrade of [false, true]) {
        const statuses = [];
        for (const granted of [["orchestration:read"], ["orchestration:operate"]]) {
          scopes = granted;
          forwarded = [];
          const response = await handler(
            new Request("http://t3.test/api/device-hub" + path, {
              method,
              headers: upgrade ? { upgrade: "websocket" } : {},
            }),
          );
          await response.arrayBuffer();
          statuses.push(response.status);
        }
        const [read, operate] = statuses;
        const scope =
          read === 403 ? "orchestration:operate" : operate === 403 ? "orchestration:read" : null;
        rows.push({ path, method, upgrade, scope, status: scope ? null : read });
      }
  scopes = ["orchestration:read"];
  forwarded = [];
  const response = await handler(
    new Request(
      "http://t3.test/api/device-hub/api/devices?wsTicket=private&hostId=local&device=a%2Fb&q=a+b",
      {
        headers: {
          authorization: "Bearer private",
          cookie: "private=secret",
          dpop: "private",
          host: "t3.test",
          origin: "http://t3.test",
          "x-fixture": "preserved",
          "accept-encoding": "gzip",
        },
      },
    ),
  );
  await response.arrayBuffer();
  rows.push({ headers: true, forwarded });
} finally {
  await dispose();
}
const file = root + "rust/crates/server/tests/fixtures/device-proxy.jsonl";
writeFileSync(file + ".tmp", rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(file + ".tmp", file);
console.log(JSON.stringify({ cases: rows.length }));
