fn main() {
    #[cfg(feature = "desktop")]
    if let Some(path) = std::env::var_os("T3_UI_DATA_DIR").filter(|path| !path.is_empty()) {
        dioxus::LaunchBuilder::desktop()
            .with_cfg(
                dioxus::desktop::Config::new().with_data_directory(std::path::PathBuf::from(path)),
            )
            .launch(t3_ui::App);
        return;
    }
    dioxus::launch(t3_ui::App);
}
