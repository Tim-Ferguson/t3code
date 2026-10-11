//! The addresses a browser shows for where the person is, the same paths the original's file routes
//! give (`apps/web/src/routes`): `/{environmentId}/{threadId}` for a thread, `/` for the start
//! screen, `/settings/{page}` for a settings page, `/pull-requests` and `/usage`.

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
}
impl SettingsPage {
    pub const ALL: [SettingsPage; 9] = [Self::General, Self::Projects, Self::Providers, Self::Appearance, Self::Connections, Self::Keybindings, Self::ScheduledTasks, Self::SourceControl, Self::TextGeneration];
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
        [environment, thread] if !environment.is_empty() && !thread.is_empty() && !matches!(*environment, "settings" | "draft") => {
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
    }
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
        let mut routes = vec![Route::Start, thread, Route::PullRequests, Route::Usage];
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
    fn an_address_that_names_nothing_is_not_a_route() {
        assert_eq!(parse("/settings/nope"), None);
        assert_eq!(parse("/a/b/c"), None);
        assert_eq!(parse("/usage/extra/more"), None);
        // The original's draft route has no counterpart: a draft is the start screen here.
        assert_eq!(parse("/draft/abc"), None);
    }
}
