// Test-only replacement entry in an ISOLATED benchmark snapshot, not production.
// No DOM reads or UI automation. Requires a unique macOS benchmark bundle ID:
// WKWebView on macOS does not honor Config::with_data_directory isolation.
fn main() {
    use dioxus::desktop::Config;
    use std::{path::PathBuf, time::Instant};
    let root = PathBuf::from(std::env::var_os("T3_BENCH_ROOT").expect("explicit benchmark root"));
    assert!(
        root.to_string_lossy()
            .starts_with("/private/tmp/t3port-bench-")
    );
    std::fs::create_dir_all(&root).expect("benchmark directory");
    let start = Instant::now();
    let path = root.join("markers.jsonl");
    let record = move |name: &str| {
        use std::io::Write;
        let row = serde_json::json!({"kind":"desktop-benchmark-marker","name":name,"elapsedFromEntryMs":start.elapsed().as_secs_f64()*1000.0,"pid":std::process::id()});
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("benchmark markers");
        writeln!(file, "{row}").expect("marker write");
        println!("{row}");
    };
    record("rust-entry");
    // A tiny patched dioxus-desktop crate emits native-set-visible-completed.
    // This entry records creation only; no window reads or DOM instrumentation.
    assert_eq!(std::env::var("T3_BENCH").as_deref(), Ok("1"));
    let config = Config::new()
        .with_data_directory(root.join("webview"))
        .with_window(
            dioxus::desktop::WindowBuilder::new()
                .with_inner_size(dioxus::desktop::LogicalSize::new(1100.0, 780.0)),
        )
        .with_on_window(move |_, _| record("rust-window-created-before-webview"));
    dioxus::LaunchBuilder::desktop()
        .with_cfg(config)
        .launch(t3_ui::App);
}
