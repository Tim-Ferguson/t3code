// Development oracle: Node24, unchanged original source and dependencies.
// node rust/tools/generate_acp_client_policy_fixtures.mjs
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  acpPermissionDisposition,
  acpClientExecuteDisposition,
  acpMcpToolApprovalElicitationDisposition,
  makeAcpClientPolicyGrants,
} from "../../apps/server/src/provider/acp/AcpClientPolicy.ts";
const root = fs.mkdtempSync(path.join(os.tmpdir(), "t3port-acp-client-policy-"));
const workspace = path.join(root, "workspace"),
  extra = path.join(root, "extra"),
  outside = path.join(root, "outside");
for (const dir of [workspace, extra, outside]) fs.mkdirSync(dir);
fs.writeFileSync(path.join(workspace, "file"), "inside");
fs.writeFileSync(path.join(outside, "file"), "outside");
fs.symlinkSync(outside, path.join(workspace, "escape"));
fs.symlinkSync(path.join(outside, "missing"), path.join(workspace, "broken"));
const fixtures = [];
const placeholder = (value) => JSON.parse(JSON.stringify(value).split(root).join("$ROOT"));
const add = (operation, input, output) =>
  fixtures.push({ operation, input: placeholder(input), output });
try {
  for (const runtimeMode of ["approval-required", "auto-accept-edits", "full-access"])
    for (const approvalPolicy of [undefined, "never", "on-request", null])
      for (const sandboxPolicy of [
        undefined,
        null,
        {},
        { type: null },
        { type: 4 },
        { type: "" },
        { type: "readOnly" },
        { type: "workspaceWrite", writableRoots: [extra] },
        { type: "dangerFullAccess" },
        { type: "externalSandbox" },
        { type: "future" },
      ]) {
        const policy = { runtimeMode, cwd: workspace, approvalPolicy, sandboxPolicy };
        for (const kind of [
          undefined,
          "read",
          "search",
          "think",
          "edit",
          "delete",
          "move",
          "execute",
          "fetch",
          "other",
          "READ",
        ]) {
          const request = {
            sessionId: "session",
            toolCall: { toolCallId: "tool", kind, locations: [{ path: "file" }] },
            options: [],
          };
          add("permission", { policy, request }, acpPermissionDisposition(policy, request));
        }
        add("execute", policy, acpClientExecuteDisposition(policy));
      }
  const policy = {
    runtimeMode: "full-access",
    cwd: workspace,
    approvalPolicy: "never",
    sandboxPolicy: { type: "workspaceWrite", writableRoots: [extra] },
  };
  for (const locations of [
    undefined,
    null,
    [],
    [{ path: "file" }],
    [{ path: "new/nested/file" }],
    [{ path: path.join(extra, "new/file") }],
    [{ path: "../outside/file" }],
    [{ path: "escape/file" }],
    [{ path: "escape/new/file" }],
    [{ path: "escape/../outside/file" }],
    [{ path: "broken" }],
    [{ path: " " }],
    [{ path: "file" }, { path: path.join(outside, "file") }],
    [{ path: "new/../file" }],
    [{ path: "nonexistent/../new" }],
    [{ path: "nonexistent/./child/../file" }],
    [{ path: "nonexistent/../../outside/file" }],
    [{ path: "escape/../new" }],
    [{ path: "escape/../workspace/new" }],
    [{ path: "escape/missing/../../new" }],
    [{ path: "nonexistent/../escape/../new" }],
    [{ path: "file/../other" }],
  ]) {
    const request = {
      sessionId: "session",
      toolCall: { toolCallId: "tool", kind: "edit", locations },
      options: [],
    };
    add("permission", { policy, request }, acpPermissionDisposition(policy, request));
  }
  for (const cwd of [null, " ", workspace])
    for (const roots of [[], [1], ["../extra"], [path.join(workspace, "broken")]]) {
      const policy = {
        runtimeMode: "full-access",
        cwd,
        approvalPolicy: "never",
        sandboxPolicy: { type: "workspaceWrite", writableRoots: roots },
      };
      const request = {
        toolCall: { kind: "edit", locations: [{ path: path.join(extra, "new") }] },
      };
      add("permission", { policy, request }, acpPermissionDisposition(policy, request));
    }
  for (const approvalPolicy of [undefined, "never", null])
    for (const runtimeMode of ["approval-required", "full-access"])
      for (const mode of ["form", "url", undefined])
        for (const tagged of [true, false])
          for (const nativeId of [undefined, "mcp_tool_call_approval_x", "other"]) {
            const policy = { runtimeMode, cwd: workspace, approvalPolicy };
            const request = { mode, _meta: tagged ? { codex_approval_kind: "mcp_tool_call" } : {} };
            add(
              "mcp",
              { policy, request, nativeId },
              acpMcpToolApprovalElicitationDisposition(policy, request, nativeId) ?? null,
            );
          }
  for (const sequence of [
    [
      { query: null },
      { query: "one" },
      { kind: "file-change", scope: "session", turnKey: "one" },
      { query: "one" },
      { kind: "command", scope: "turn", turnKey: "one" },
      { query: null },
      { query: "one" },
      { query: "two" },
      { kind: "command", scope: "turn", turnKey: "two" },
      { query: "one" },
      { query: "two" },
    ],
    [
      { kind: "command", scope: "session", turnKey: "one" },
      { query: null },
      { query: "one" },
      { query: "two" },
      { kind: "command", scope: "turn", turnKey: "three" },
      { query: "one" },
    ],
  ]) {
    const grants = makeAcpClientPolicyGrants();
    const output = sequence.map((row) =>
      "query" in row ? grants.allowsExecute(row.query) : (grants.recordApproval(row), null),
    );
    add("grants", sequence, output);
  }
  fs.writeFileSync(
    new URL("../crates/server/tests/fixtures/acp-client-policy.jsonl", import.meta.url),
    fixtures.map((value) => JSON.stringify(value)).join("\n") + "\n",
  );
  console.log(`Generated ${fixtures.length} source ACP client policy/grants cases`);
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
