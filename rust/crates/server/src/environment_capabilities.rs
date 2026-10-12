//! The `capabilities` this server advertises in its environment descriptor
//! (`environment/ServerEnvironment.ts`). Clients gate controls on them, so an entry is present
//! only for a feature this server has ported; the original advertises a few more (see the list
//! in `tests` for the ones held back and why).
use serde_json::{Value, json};
use t3_contracts::PROVIDER_SEND_TURN_MAX_FILE_BYTES;

pub fn advertised() -> Value {
    advertised_with(None)
}

/// With the update entries (`serverSelfUpdate`, `serverSelfUpdateProgress`, `desktopAppUpdate`) a server that can be updated
/// advertises (`self_update_rpc::capabilities`); an installation the npm package manager owns (`serverInstallation`) is not a
/// thing a native binary has.
pub fn advertised_with(update: Option<crate::self_update_rpc::UpdateCapabilities>) -> Value {
    advertised_for(update, false)
}

/// `advertised_with`, and `serverBrowser` when the server's own browser is switched on (`server_browser::SWITCH`). The original always
/// advertises it; here it is only true of a server that will run the browser, because clients open `server` tabs on it.
pub fn advertised_for(update: Option<crate::self_update_rpc::UpdateCapabilities>, server_browser: bool) -> Value {
    let mut capabilities = base();
    if server_browser {
        capabilities["serverBrowser"] = json!(true);
    }
    if let Some(update) = update {
        if let Some(self_update) = update.self_update {
            capabilities["serverSelfUpdate"] = serde_json::to_value(self_update).unwrap();
        }
        if update.progress {
            capabilities["serverSelfUpdateProgress"] = json!(true);
        }
        if update.desktop_app_update {
            capabilities["desktopAppUpdate"] = json!(true);
        }
    }
    capabilities
}

fn base() -> Value {
    json!({
        // Shell frames carry each project's repository identity once its root has resolved (`shell_enrichment`).
        "repositoryIdentity": true,
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
        // Settle releases the thread's sessions, cancels the deliveries that only wake its agent and ends its
        // watches; a message brings it back; the sweep settles a stale one (`thread_settlement`).
        "threadSettlement": true,
        "threadAutoSettlement": true,
        // `continueThreadsAfterServerUpdate` resumes the turn a restart cut (`restart_continuation`).
        "threadRestartContinuation": true,
        // `thread.pull-request.watch` and the watcher that wakes the agent (`pull_request_watcher`).
        "threadPullRequestWatch": true,
        // `pullRequests.checks` and the checks of a pull request's detail.
        "pullRequestChecks": true,
        // The usage page applies the custom rates and the model mappings the settings hold.
        "usagePriceOverrides": true,
        "usageModelAliases": true,
        // `<state dir>/themes` is watched and streamed, and `t3 theme` publishes into it (`environment_theme`).
        "environmentThemes": true,
        // The descriptor carries the machine kind (`environment_host`); the icon itself is a client setting.
        "environmentIcon": true,
        // `storage_cleanup`: the hourly sweep of worktrees, browser artifacts and rotated logs, and the worktree recreated at turn start.
        "storageCleanup": true,
        // A client that sees this sends the user's intent and lets the server resolve the rest: `deliveryIntent` with
        // `start_immediately`/`queue_after_active` (`message_steering::resolve_dispatch_mode` picks steer, restart or queue against
        // the active run), a `titleSeed` on the first message, `thread.model-selection.set` for any provider, and
        // `checkpoint.rollback` by scope and checkpoint id.
        "serverResolvedCommandContext": true,
        "pullRequestStackActions": true,
        "projectWorktreeCleanup": true,
        // The quota of the configured `usageLimitSources`, streamed as `usageLimitSourcesUpdated` (`usage_limit_sources`).
        "usageLimitSources": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The update entries follow who manages the server: a desktop-managed one advertises the update and, with the app's control
    /// channel, its progress and the desktop update; a web server advertises none.
    #[test]
    fn the_update_entries_follow_who_manages_the_server() {
        let describe = |desktop: bool, control: bool| {
            let capabilities = advertised_with(Some(crate::self_update_rpc::capabilities(desktop, control, false)));
            let descriptor = json!({"environmentId":"environment","label":"Rust","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":capabilities});
            serde_json::from_value::<t3_contracts::ExecutionEnvironmentDescriptor>(descriptor).expect("a valid descriptor");
            capabilities
        };
        let web = describe(false, false);
        assert!(["serverSelfUpdate", "serverSelfUpdateProgress", "desktopAppUpdate", "serverInstallation"].iter().all(|key| web.get(key).is_none()));
        let bare = describe(true, false);
        assert_eq!(bare["serverSelfUpdate"], "desktop-managed");
        assert!(bare.get("serverSelfUpdateProgress").is_none() && bare.get("desktopAppUpdate").is_none());
        let app = describe(true, true);
        assert_eq!((&app["serverSelfUpdate"], &app["serverSelfUpdateProgress"], &app["desktopAppUpdate"]), (&json!("desktop-managed"), &json!(true), &json!(true)));
    }

    /// Held back because their feature is not (fully) ported: the
    /// browser, and the self-update entries.
    #[test]
    fn the_advertised_capabilities_are_valid_and_leave_out_what_is_not_ported() {
        let descriptor = json!({"environmentId":"environment","label":"Rust","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":advertised()});
        let decoded: t3_contracts::ExecutionEnvironmentDescriptor = serde_json::from_value(descriptor.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap()["capabilities"], descriptor["capabilities"]);
        for ported in ["threadSettlement", "threadAutoSettlement", "threadRestartContinuation", "threadPullRequestWatch", "pullRequestChecks", "usagePriceOverrides", "usageModelAliases", "repositoryIdentity", "storageCleanup", "projectWorktreeCleanup", "serverResolvedCommandContext", "pullRequestStackActions", "usageLimitSources"] {
            assert_eq!(advertised()[ported], true, "{ported}");
        }
        for held_back in ["serverBrowser"] {
            assert!(advertised().get(held_back).is_none(), "{held_back}");
        }
    }

    /// Only a server that runs the browser says so, and the descriptor with it still decodes.
    #[test]
    fn the_server_browser_is_advertised_only_when_it_is_switched_on() {
        assert!(advertised_for(None, false).get("serverBrowser").is_none());
        let capabilities = advertised_for(None, true);
        assert_eq!(capabilities["serverBrowser"], true);
        let descriptor = json!({"environmentId":"environment","label":"Rust","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":capabilities});
        let decoded: t3_contracts::ExecutionEnvironmentDescriptor = serde_json::from_value(descriptor).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap()["capabilities"]["serverBrowser"], true);
    }
}
