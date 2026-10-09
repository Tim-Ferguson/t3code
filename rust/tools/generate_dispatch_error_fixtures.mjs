// Development oracle: runs the original `ws.ts` error mapping for refused commands. A refused
// `dispatchCommand` becomes `OrchestrationV2DispatchCommandError` whose message is
// `userFacingDispatchErrorMessage(cause)`; the cause is the original's own error object. The
// Rust port replays every row through `dispatch_error.rs`.
import { renameSync, writeFileSync } from "node:fs";
import * as Schema from "../../apps/server/node_modules/effect/dist/Schema.js";
import {
  OrchestrationV2DispatchCommandError,
  OrchestrationV2ThreadLaunchError,
} from "../../packages/contracts/src/orchestrationV2.ts";
import { userFacingDispatchErrorMessage } from "../../apps/server/src/orchestration-v2/UserFacingErrors.ts";
import {
  OrchestratorCommandPreviouslyRejectedError,
  OrchestratorDispatchError,
} from "../../apps/server/src/orchestration-v2/Orchestrator.ts";
import { ThreadLaunchError } from "../../apps/server/src/orchestration-v2/ThreadLaunchService.ts";
import { AttachmentClaimError } from "../../apps/server/src/orchestration-v2/AttachmentClaims.ts";

const encodeDispatch = Schema.encodeSync(OrchestrationV2DispatchCommandError);
const encodeLaunch = Schema.encodeSync(OrchestrationV2ThreadLaunchError);
const plain = (value) => JSON.parse(JSON.stringify(value));

// What `ws.ts` builds for any failure of a dispatched command.
const dispatchEnvelope = (command, cause) => {
  const detail = userFacingDispatchErrorMessage(cause);
  return plain(
    encodeDispatch(
      new OrchestrationV2DispatchCommandError({
        commandId: command.commandId,
        commandType: command.type,
        message: detail ?? "Failed to dispatch orchestration V2 command",
        ...(detail === undefined ? {} : { detail }),
        cause,
      }),
    ),
  );
};

const rows = [];
const reasons = [
  "Thread is deleted or archived.",
  "Run is not active.",
  "Claude cannot queue messages behind an active run.",
  "Native provider command thread.fork is not yet available.",
  "Attachment 'shot.png' cannot be sent: stored size does not match.",
];
for (const type of ["message.dispatch", "thread.archive", "checkpoint.rollback"]) {
  for (const reason of reasons) {
    const command = { type, commandId: `command:${type}` };
    rows.push({
      name: "refused-at-admission",
      command,
      refusal: { kind: "reason", reason },
      envelope: dispatchEnvelope(
        command,
        new OrchestratorDispatchError({
          ...{ commandId: command.commandId, commandType: type },
          cause: reason,
        }),
      ),
    });
  }
}
{
  const command = { type: "message.dispatch", commandId: "command:replayed" };
  rows.push({
    name: "previously-rejected",
    command,
    // The receipt keeps `cause.message`, the generic text of the original refusal.
    refusal: {
      kind: "previously-rejected",
      detail: "Failed to dispatch orchestration command message.dispatch (command:replayed).",
    },
    envelope: dispatchEnvelope(
      command,
      new OrchestratorCommandPreviouslyRejectedError({
        commandId: command.commandId,
        commandType: command.type,
        detail: "Failed to dispatch orchestration command message.dispatch (command:replayed).",
      }),
    ),
  });
}
{
  const command = { type: "message.dispatch", commandId: "command:silent" };
  rows.push({
    name: "no-usable-message",
    command,
    refusal: { kind: "reason", reason: "" },
    envelope: dispatchEnvelope(
      command,
      new OrchestratorDispatchError({ commandId: command.commandId, commandType: command.type }),
    ),
  });
}
{
  const input = { commandId: "launch:claim", projectId: "project:1" };
  const cause = new AttachmentClaimError({
    message: "Uploaded attachments need a thread id at launch.",
  });
  rows.push({
    name: "launch-attachment-claim",
    input,
    envelope: plain(
      encodeLaunch(
        new OrchestrationV2ThreadLaunchError({
          commandId: input.commandId,
          projectId: input.projectId,
          message: cause.message,
          cause,
        }),
      ),
    ),
  });
}
{
  // The original's message for a failed launch is constant and the reason lives in the cause
  // chain; the Rust envelope puts the reason in the message (a documented divergence).
  const input = { commandId: "launch:refused", projectId: "project:1" };
  const reason =
    "Only an empty active thread in the target project can change workspace during launch.";
  const cause = new ThreadLaunchError({
    operation: "update-thread",
    commandId: input.commandId,
    projectId: input.projectId,
    threadId: "thread:1",
    cause: new OrchestratorDispatchError({
      commandId: `${input.commandId}`,
      commandType: "thread.metadata.update",
      cause: reason,
    }),
  });
  rows.push({
    name: "launch-refused",
    input,
    reason,
    envelope: plain(
      encodeLaunch(
        new OrchestrationV2ThreadLaunchError({
          commandId: input.commandId,
          projectId: input.projectId,
          message: "Failed to launch thread",
          cause,
        }),
      ),
    ),
  });
}
const target = new URL("../crates/server/tests/fixtures/dispatch-errors.jsonl", import.meta.url);
const staging = new URL(
  "../crates/server/tests/fixtures/dispatch-errors.jsonl.tmp",
  import.meta.url,
);
writeFileSync(staging, rows.map((row) => JSON.stringify(row)).join("\n") + "\n");
renameSync(staging, target);
console.log(`wrote ${rows.length} rows`);
