//! Settings → `t3` command: puts the app's bundled command line on PATH, or takes it off again (`CliCommandSettingsRow.tsx`).
//! The logic and the files it writes are `t3_desktop::cli_command`; this is the row. It is drawn on the desktop app only, and
//! only where the app has a command to install.
use dioxus::prelude::*;
use t3_desktop::cli_command::{self, State};

#[component]
pub(crate) fn Row() -> Element {
    let environment = use_hook(crate::desktop_shell::cli_environment);
    let Some(environment) = environment else { return rsx! {} };
    let mut state = use_signal({
        let environment = environment.clone();
        move || cli_command::state(&environment)
    });
    let mut failure = use_signal(|| None::<String>);
    let mut pending = use_signal(|| false);
    let current = state.read().clone();
    if !current.supported {
        return rsx! {};
    }
    let installed = current.installed_path.clone();
    let description = match (&installed, current.on_path) {
        (None, _) => "Run T3 Code's CLI as `t3` from any terminal.".to_owned(),
        (Some(path), true) => format!("Installed at {}. Open a new terminal to use it.", path.display()),
        (Some(path), false) => format!("Installed at {}, which is not on your PATH yet. Add its folder to your PATH to run `t3`.", path.display()),
    };
    let remove = installed.is_some();
    let label = if remove { "Remove" } else { "Install" };
    rsx! {
        div { class: "field-row", id: "cli-command",
            div { strong { "t3 command" } p { class: "muted", "{description}" } }
            button { r#type: "button", "aria-label": "{label} the t3 command", disabled: *pending.read(),
                onclick: move |_| {
                    if *pending.peek() { return; }
                    pending.set(true);
                    let environment = environment.clone();
                    // A few file operations in the person's own folders; nothing here waits on a program.
                    let next: Result<State, String> = if remove { cli_command::uninstall(&environment) } else { cli_command::install(&environment) };
                    match next {
                        Ok(next) => { failure.set(None); state.set(next); }
                        Err(error) => failure.set(Some(if remove { format!("Could not remove t3. {error}") } else { format!("Could not install t3. {error}") })),
                    }
                    pending.set(false);
                },
                "{label}" }
        }
        if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
    }
}
