//! The addresses a browser shows for where the person is, the same paths the original's file routes
//! give (`apps/web/src/routes`): `/{environmentId}/{threadId}` for a thread, `/` for the start
//! screen, `/draft/{draftId}` for a draft of a new thread, `/settings/{page}` for a settings page, `/pull-requests` and `/usage`.

/// A settings page, by the segment after `/settings/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Providers,
    Appearance,
    Connections,
    Keybindings,
    ScheduledTasks,
    SourceControl,
    /// Not a page of its own in the original; the port keeps it where it lists it.
    TextGeneration,
    Projects,
    /// The server's live processes and trace summary (`/settings/diagnostics`).
    Diagnostics,
    /// Worktree, artifact and log cleanup (`/settings/storage`).
    Storage,
    /// Archived threads (`/settings/archived`).
    Archived,
    /// The browser and device hubs, and the machines that host simulators (`/settings/integrations`).
    Integrations,
}
impl SettingsPage {
    pub const ALL: [SettingsPage; 13] = [Self::General, Self::Projects, Self::Providers, Self::Appearance, Self::Connections, Self::Keybindings, Self::ScheduledTasks, Self::SourceControl, Self::TextGeneration, Self::Diagnostics, Self::Storage, Self::Integrations, Self::Archived];
    pub fn segment(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Providers => "providers",
            Self::Appearance => "appearance",
            Self::Connections => "connections",
            Self::Keybindings => "keybindings",
            Self::ScheduledTasks => "scheduled-tasks",
            Self::SourceControl => "source-control",
            Self::TextGeneration => "text-generation",
            Self::Projects => "projects",
            Self::Diagnostics => "diagnostics",
            Self::Storage => "storage",
            Self::Archived => "archived",
            Self::Integrations => "integrations",
        }
    }
    fn of(segment: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|page| page.segment() == segment)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The start screen, with no thread open.
    Start,
    Thread { environment: String, thread: String },
    Settings(SettingsPage),
    PullRequests,
    Usage,
    /// `/draft/<id>`: a draft of a new thread, at its own address.
    Draft(String),
    /// `/welcome`: the first-run wizard, which overlays whatever else is shown.
    Welcome,
    /// `/projects/<key>`: the original redirects to that project's settings (`/settings/projects?project=<key>`); so does this.
    Project(String),
}

/// The route an address names, or `None` for one that names nothing the app shows. A query or fragment
/// and a trailing slash do not matter.
pub fn parse(address: &str) -> Option<Route> {
    let path = address.split(['?', '#']).next().unwrap_or_default();
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let segments = match segments.as_slice() {
        [rest @ .., ""] => rest,
        all => all,
    };
    match segments {
        [] => Some(Route::Start),
        ["settings"] => Some(Route::Settings(SettingsPage::General)),
        ["settings", page] => SettingsPage::of(page).map(Route::Settings),
        ["pull-requests"] => Some(Route::PullRequests),
        ["usage"] => Some(Route::Usage),
        ["welcome"] => Some(Route::Welcome),
        ["draft", id] if !id.is_empty() => Some(Route::Draft(decode(id)?)),
        ["projects", key] if !key.is_empty() => Some(Route::Project(decode(key)?)),
        [environment, thread] if !environment.is_empty() && !thread.is_empty() && !matches!(*environment, "settings" | "draft" | "welcome") => {
            Some(Route::Thread { environment: decode(environment)?, thread: decode(thread)? })
        }
        _ => None,
    }
}

/// The address of a route.
pub fn path(route: &Route) -> String {
    match route {
        Route::Start => "/".to_owned(),
        Route::Thread { environment, thread } => format!("/{}/{}", encode(environment), encode(thread)),
        Route::Settings(page) => format!("/settings/{}", page.segment()),
        Route::PullRequests => "/pull-requests".to_owned(),
        Route::Usage => "/usage".to_owned(),
        Route::Welcome => "/welcome".to_owned(),
        Route::Project(key) => format!("/projects/{}", encode(key)),
        Route::Draft(id) => format!("/draft/{}", encode(id)),
    }
}

/// The project a settings address names in its query (`/settings/projects?project=<key>`), if it names one.
pub fn project_query(address: &str) -> Option<String> {
    let query = address.split('#').next()?.split_once('?')?.1;
    query.split('&').find_map(|pair| pair.strip_prefix("project=")).and_then(decode).filter(|key| !key.is_empty())
}

/// One path segment, with everything outside the unreserved set escaped.
fn encode(segment: &str) -> String {
    let mut out = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
fn decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = segment.get(at + 1..at + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_round_trips_through_its_address() {
        let thread = Route::Thread { environment: "env-1".into(), thread: "0a1b-2c".into() };
        assert_eq!(path(&thread), "/env-1/0a1b-2c");
        let mut routes = vec![Route::Start, thread, Route::PullRequests, Route::Usage, Route::Project("env:project-1".into()), Route::Draft("0f3a-9c".into()), Route::Welcome];
        routes.extend(SettingsPage::ALL.map(Route::Settings));
        for route in routes {
            assert_eq!(parse(&path(&route)), Some(route.clone()), "{route:?}");
        }
    }

    #[test]
    fn a_segment_with_reserved_characters_is_escaped_and_restored() {
        let route = Route::Thread { environment: "a b/c".into(), thread: "t?#é".into() };
        assert_eq!(path(&route), "/a%20b%2Fc/t%3F%23%C3%A9");
        assert_eq!(parse(&path(&route)), Some(route));
        assert_eq!(parse("/env/%zz"), None);
        assert_eq!(parse("/env/%FF"), None);
    }

    #[test]
    fn the_original_settings_paths_and_the_forgiving_forms_are_read() {
        assert_eq!(parse("/settings/scheduled-tasks"), Some(Route::Settings(SettingsPage::ScheduledTasks)));
        assert_eq!(parse("/settings/source-control/"), Some(Route::Settings(SettingsPage::SourceControl)));
        assert_eq!(parse("/settings"), Some(Route::Settings(SettingsPage::General)));
        assert_eq!(parse(""), Some(Route::Start));
        assert_eq!(parse("/?token=abc#frag"), Some(Route::Start));
        assert_eq!(parse("/env/thread?x=1"), Some(Route::Thread { environment: "env".into(), thread: "thread".into() }));
    }

    #[test]
    fn a_project_address_and_a_settings_query_name_a_project() {
        assert_eq!(parse("/projects/p%3A1"), Some(Route::Project("p:1".into())));
        assert_eq!(parse("/settings/storage"), Some(Route::Settings(SettingsPage::Storage)));
        assert_eq!(parse("/settings/archived"), Some(Route::Settings(SettingsPage::Archived)));
        assert_eq!(project_query("/settings/projects?project=p%3A1&machine=x").as_deref(), Some("p:1"));
        assert_eq!(project_query("/settings/projects?machine=x"), None);
        assert_eq!(project_query("/settings/projects"), None);
    }

    #[test]
    fn an_address_that_names_nothing_is_not_a_route() {
        assert_eq!(parse("/settings/nope"), None);
        assert_eq!(parse("/a/b/c"), None);
        assert_eq!(parse("/usage/extra/more"), None);
        assert_eq!(parse("/draft"), None);
        assert_eq!(parse("/draft/abc/more"), None);
        assert_eq!(parse("/draft/abc"), Some(Route::Draft("abc".into())));
    }
}
