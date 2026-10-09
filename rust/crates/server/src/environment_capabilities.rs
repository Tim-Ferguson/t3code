//! The `capabilities` this server advertises in its environment descriptor
//! (`environment/ServerEnvironment.ts`). Clients gate controls on them, so an entry is present
//! only for a feature this server has ported; the original advertises a few more (see the list
//! in `tests` for the ones held back and why).
use serde_json::{Value, json};
use t3_contracts::PROVIDER_SEND_TURN_MAX_FILE_BYTES;

pub fn advertised() -> Value {
    json!({
        "repositoryIdentity": false,
        "connectionProbe": true,
        "attachmentUploads": true,
        "questionAttachments": true,
        "fileAttachments": {"maxUploadBytes": PROVIDER_SEND_TURN_MAX_FILE_BYTES},
        "pullRequests": true,
        "inlineMessageContext": true,
        "requiredWorktreeBootstrap": true,
        "worktreesDirectory": true,
        "projectSettingsOverrides": true,
        "threadSnooze": true,
        "threadPinning": true,
        "threadPinReorder": true,
        "threadActiveReorder": true,
        "threadAutoSettleOptOut": true,
        // The original sets this unconditionally: `regenerateTitle` on `thread.metadata.update`
        // is understood whatever providers are installed (a failed generation keeps the title).
        "threadTitleRegeneration": true,
        "threadVisitedTracking": true,
        "threadPullRequests": true,
        "threadPullRequestLinking": true,
        "projectCloneTracking": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Held back because their feature is not (fully) ported: settlement refuses threads with
    /// live sessions, no cleanup of stored worktrees or logs, no restart continuation, usage
    /// accounting, themes, pull request checks, stack actions and
    /// watches, server-resolved command context, an environment icon, the browser, and the
    /// self-update entries.
    #[test]
    fn the_advertised_capabilities_are_valid_and_leave_out_what_is_not_ported() {
        let descriptor = json!({"environmentId":"environment","label":"Rust","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":advertised()});
        let decoded: t3_contracts::ExecutionEnvironmentDescriptor = serde_json::from_value(descriptor.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap()["capabilities"], descriptor["capabilities"]);
        for held_back in ["threadSettlement", "threadAutoSettlement", "storageCleanup", "projectWorktreeCleanup", "threadRestartContinuation", "usageLimitSources", "environmentThemes", "pullRequestChecks", "pullRequestStackActions", "threadPullRequestWatch", "serverResolvedCommandContext", "environmentIcon", "serverBrowser"] {
            assert!(advertised().get(held_back).is_none(), "{held_back}");
        }
    }
}
