fn main() {
    #[cfg(target_arch = "wasm32")]
    t3_ui::install_panic_screen();
    // The desktop app opens its own window, with its menu, its saved size and the server it starts itself.
    #[cfg(feature = "desktop")]
    {
        t3_ui::launch_desktop(t3_ui::App);
        return;
    }
    #[cfg(not(feature = "desktop"))]
    dioxus::launch(t3_ui::App);
}
