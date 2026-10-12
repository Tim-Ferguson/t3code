// Development-only oracle: the control address `t3 app` and the desktop shell share (packages/shared/src/desktopAppControl.ts),
// and the original's decoding of the activation protocol lines (packages/contracts/src/desktopAppActivation.ts), over the cases
// `crates/remote/tests/desktop_app_control.rs` replays.
import { writeFileSync } from "node:fs";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import { resolveDesktopAppControlAddress } from "../../packages/shared/src/desktopAppControl.ts";
import {
  DesktopAppActivationRequest,
  DesktopAppActivationResponse,
} from "../../packages/contracts/src/desktopAppActivation.ts";

const join = (...segments) => segments.join("/");
const addresses = [];
for (const stateDir of [
  "/Users/me/.t3/userdata",
  "/Users/me/.t3/dev",
  "/home/user/.t3/userdata",
  "/tmp/a very long state directory/".repeat(8),
  "relative/dir",
  "/Users/mé/.t3/userdata",
  "C:\\Users\\me\\.t3\\userdata",
  "",
]) {
  for (const platform of ["darwin", "linux", "win32"]) {
    for (const userId of [undefined, 0, 501]) {
      const found = resolveDesktopAppControlAddress({
        stateDir,
        platform,
        tempDir: "/var/folders/xx/T",
        userId,
        joinPath: join,
      });
      addresses.push({
        stateDir,
        platform,
        tempDir: "/var/folders/xx/T",
        userId: userId ?? null,
        ...found,
      });
    }
  }
}

const isRequest = Schema.is(DesktopAppActivationRequest);
const isResponse = Schema.is(DesktopAppActivationResponse);
const requests = [
  { version: 1, requestId: "a", type: "open-workspace", workspaceRoot: "/w", platform: "darwin" },
  {
    version: 1,
    requestId: " a ",
    type: "open-workspace",
    workspaceRoot: " /w ",
    platform: "linux",
  },
  {
    version: 1,
    requestId: "a",
    type: "open-workspace",
    workspaceRoot: "/w",
    platform: "win32",
    extra: 1,
  },
  { version: 2, requestId: "a", type: "open-workspace", workspaceRoot: "/w", platform: "linux" },
  { version: 1, requestId: "", type: "open-workspace", workspaceRoot: "/w", platform: "linux" },
  { version: 1, requestId: "   ", type: "open-workspace", workspaceRoot: "/w", platform: "linux" },
  { version: 1, requestId: "a", type: "open-workspace", workspaceRoot: "", platform: "linux" },
  { version: 1, requestId: "a", type: "close", workspaceRoot: "/w", platform: "linux" },
  { version: 1, requestId: "a", type: "open-workspace", workspaceRoot: "/w", platform: "freebsd" },
  { version: 1, requestId: "a", type: "open-workspace", platform: "linux" },
  { version: 1, requestId: 3, type: "open-workspace", workspaceRoot: "/w", platform: "linux" },
  { version: "1", requestId: "a", type: "open-workspace", workspaceRoot: "/w", platform: "linux" },
  [],
  null,
  "text",
];
const responses = [
  { version: 1, requestId: "a", ok: true, projectId: "p", threadId: "t" },
  { version: 1, requestId: " a ", ok: true, projectId: " p ", threadId: " t ", extra: true },
  { version: 1, requestId: "a", ok: true, projectId: "p" },
  { version: 1, requestId: "a", ok: true, projectId: "", threadId: "t" },
  { version: 1, requestId: "a", ok: false, code: "project-create-failed", message: "No." },
  { version: 1, requestId: "a", ok: false, code: "internal-error", message: "  " },
  { version: 1, requestId: "a", ok: false, code: "nope", message: "x" },
  { version: 1, requestId: "a", ok: false, projectId: "p", threadId: "t" },
  { version: 1, requestId: "a", ok: true, code: "internal-error", message: "x" },
  {
    version: 1,
    requestId: "a",
    ok: false,
    code: "request-timeout",
    message: "x",
    projectId: "p",
    threadId: "t",
  },
  { version: 3, requestId: "a", ok: true, projectId: "p", threadId: "t" },
  { invalid: true },
  { version: 1, requestId: "a", ok: "true", projectId: "p", threadId: "t" },
  [],
  null,
];
const protocol = [
  ...requests.map((value) => ({ kind: "request", value, valid: isRequest(value) })),
  ...responses.map((value) => ({ kind: "response", value, valid: isResponse(value) })),
];

writeFileSync(
  new URL("../crates/remote/tests/fixtures/desktop-app-control.json", import.meta.url),
  JSON.stringify({ addresses, protocol }),
);
console.log(JSON.stringify({ addresses: addresses.length, protocol: protocol.length }));
