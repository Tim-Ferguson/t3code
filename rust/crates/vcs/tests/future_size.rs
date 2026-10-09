//! Bounds on the futures of the git operations that status and the ref picker await.
//!
//! A future embeds every future it awaits, so one large leaf makes each operation above it
//! large, and a debug build then needs a great deal of stack to build and move them (a request
//! overflowed a 2 MiB thread). Before `TtlCache::get` boxed its loader, `list_refs` was 15 KiB
//! and `status_details_local` 11 KiB in a debug build; they are now about 1 KiB and 3 KiB.
//! The limit leaves room to grow but fails on a return to that shape: box the large future
//! where it is awaited instead of raising it.
mod common;

use common::live_core;
use std::path::Path;
use t3_vcs::{
    status::LocalStatusOptions,
    wire::VcsListRefsInput,
};

const LIMIT_BYTES: usize = 8 * 1024;

fn assert_fits<F>(name: &str, future: &F) {
    let size = std::mem::size_of_val(future);
    assert!(size <= LIMIT_BYTES, "the future of {name} is {size} bytes (limit {LIMIT_BYTES})");
}

#[test]
fn status_and_ref_listing_futures_stay_small() {
    let cwd = Path::new("/");
    let core = live_core();
    assert_fits(
        "list_refs",
        &core.list_refs(&VcsListRefsInput {
            cwd: cwd.to_string_lossy().into_owned(),
            ..VcsListRefsInput::default()
        }),
    );
    assert_fits(
        "status_details_local",
        &core.status_details_local(
            cwd,
            LocalStatusOptions {
                include_divergence: true,
                include_branch_changes: true,
            },
        ),
    );
    assert_fits("status", &core.status(cwd));
}
