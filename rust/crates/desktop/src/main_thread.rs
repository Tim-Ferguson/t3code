//! Work for the thread the window toolkit runs on. The app's other threads (the control socket's, the signal listener's) bring the
//! window forward or end the app without asking the page to do it: the page stops running while its window is hidden or covered
//! (the webview does not draw, and the page waits for it to have drawn), so what it is asked to do is not done until the window is
//! seen, and the request is what would show it.

/// Runs `work` on the main thread, soon, without waiting for it. On macOS the window toolkit's calls on a window have to be made
/// there (some do it for themselves, some do not); elsewhere its windows can be called from any thread and `work` runs at once.
pub fn on_main_thread(work: impl FnOnce() + Send + 'static) {
    #[cfg(target_os = "macos")]
    dispatch2::DispatchQueue::main().exec_async(work);
    #[cfg(not(target_os = "macos"))]
    work();
}
