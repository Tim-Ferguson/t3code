// Development oracle uses the unchanged proxy and original FetchHttpClient.
import { createServer } from "node:http";
import { gzipSync, deflateSync, brotliCompressSync } from "node:zlib";
import { writeFileSync, renameSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url));
const Effect = await import(
  pathToFileURL(root + "packages/contracts/node_modules/effect/dist/Effect.js")
);
const Layer = await import(
  pathToFileURL(root + "packages/contracts/node_modules/effect/dist/Layer.js")
);
const { FetchHttpClient, HttpRouter } = await import(
  pathToFileURL(root + "packages/contracts/node_modules/effect/dist/http/index.js")
);
const auth = await import(pathToFileURL(root + "apps/server/src/auth/EnvironmentAuth.ts"));
const devices = await import(pathToFileURL(root + "apps/server/src/device/DeviceService.ts"));
const proxy = await import(pathToFileURL(root + "apps/server/src/device/DeviceHubProxy.ts"));
const body = Buffer.from("fixture Unicode 👋\u0000stream");
const compressed = {
  gzip: gzipSync(body),
  deflate: deflateSync(body),
  br: brotliCompressSync(body),
};
const requests = [];
const server = createServer((request, response) => {
  requests.push(request.headers);
  const encoding = new URL(request.url, "http://localhost").searchParams.get("encoding");
  response.writeHead(200, {
    "content-encoding": encoding,
    "content-type": "application/octet-stream",
  });
  response.end(compressed[encoding]);
});
await new Promise((resolve, reject) => {
  server.once("error", reject);
  server.listen(0, "127.0.0.1", resolve);
});
const origin = "http://127.0.0.1:" + server.address().port;
const { handler, dispose } = HttpRouter.toWebHandler(
  proxy.layer.pipe(
    Layer.provideMerge(
      Layer.succeed(auth.EnvironmentAuth, {
        authenticateWebSocketUpgrade: () =>
          Effect.succeed({
            sessionId: "fixture",
            subject: "fixture",
            method: "bearer-access-token",
            scopes: ["orchestration:read"],
          }),
      }),
    ),
    Layer.provideMerge(
      Layer.succeed(devices.DeviceService, {
        currentReadiness: () => Effect.succeed({ hostId: "local", hub: { origin } }),
      }),
    ),
    Layer.provideMerge(FetchHttpClient.layer),
  ),
  { disableLogger: true },
);
const rows = [];
try {
  for (const encoding of Object.keys(compressed)) {
    const response = await handler(
      new Request(
        "http://t3.test/api/device-hub/vendor/serve-sim/helper/proxy/health?encoding=" + encoding,
        { headers: { "accept-encoding": "fixture-client-value" } },
      ),
    );
    const result = Buffer.from(await response.arrayBuffer());
    if (!result.equals(body)) throw new Error("Source Fetch decompression mismatch: " + encoding);
    rows.push({
      encoding,
      compressed: compressed[encoding].toString("base64"),
      body: result.toString("base64"),
      status: response.status,
      contentEncoding: response.headers.get("content-encoding"),
      cacheControl: response.headers.get("cache-control"),
      generatedAcceptEncoding: requests.at(-1)["accept-encoding"],
    });
  }
} finally {
  await dispose();
  await new Promise((resolve) => server.close(resolve));
}
const path = root + "rust/crates/server/tests/fixtures/device-proxy-compression.jsonl";
writeFileSync(path + ".tmp", rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(path + ".tmp", path);
console.log(JSON.stringify({ cases: rows.length }));
