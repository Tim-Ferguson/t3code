//! The artwork and pill that mark a Dev or Nightly environment (`SidebarStageBackdrop.tsx`, `SidebarChrome.tsx`,
//! `useEnvironmentIdentificationMode`), drawn when the "Environment identification" preference asks for it. The art is
//! static line work; its colors are the `--stage-*` variables, which a built-in theme sets for itself (`app.css`).
//! Decisions (the channel, the mode) are `t3_client::stage`.
use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use std::fmt::Write as _;
use t3_client::stage::{self, Variant};
use t3_contracts::EnvironmentIdentificationMode as Mode;

/// A wide viewBox keeps the 96-unit art height at a fixed scale while sidebar resizing reveals more horizontal canvas instead of
/// zooming the scene.
const BACKDROP_VIEW_BOX: &str = "0 0 8192 96";

const NIGHTLY_STARS: [(f32, f32, f32, f32); 17] = [
    (14.0, 10.0, 0.6, 0.85),
    (38.0, 22.0, 0.4, 0.55),
    (58.0, 8.0, 0.5, 0.7),
    (84.0, 16.0, 0.4, 0.5),
    (104.0, 7.0, 0.6, 0.8),
    (126.0, 20.0, 0.4, 0.55),
    (148.0, 11.0, 0.5, 0.7),
    (170.0, 24.0, 0.4, 0.5),
    (192.0, 9.0, 0.6, 0.8),
    (214.0, 18.0, 0.4, 0.55),
    (236.0, 8.0, 0.5, 0.7),
    (258.0, 20.0, 0.45, 0.6),
    (278.0, 11.0, 0.55, 0.75),
    (26.0, 34.0, 0.4, 0.45),
    (118.0, 34.0, 0.4, 0.45),
    (202.0, 32.0, 0.4, 0.5),
    (268.0, 34.0, 0.4, 0.45),
];
const NIGHTLY_SPARKLES: [(f32, f32); 3] = [(70.0, 28.0), (160.0, 36.0), (246.0, 26.0)];

/// The Nightly sky. `id` keeps the gradient ids of two drawings on one page apart (a hidden drawing's definitions would
/// otherwise break the references of a visible one).
fn nightly(id: &str, compact: bool) -> String {
    let view_box = if compact { "96 0 8192 96" } else { BACKDROP_VIEW_BOX };
    let (sky, glow, cloud, soft, stars, glows) = (
        format!("{id}-stage-night-sky"),
        format!("{id}-stage-night-glow"),
        format!("{id}-stage-night-cloud"),
        format!("{id}-stage-night-soft"),
        format!("{id}-stage-night-stars"),
        format!("{id}-stage-night-glows"),
    );
    let mut stars_markup = String::new();
    for (cx, cy, r, opacity) in NIGHTLY_STARS {
        let _ = write!(stars_markup, r#"<circle cx="{cx}" cy="{cy}" r="{r}" fill-opacity="{opacity}"/>"#);
    }
    let mut sparkles = String::new();
    for (x, y) in NIGHTLY_SPARKLES {
        let _ = write!(sparkles, r#"<g><path d="M{} {y}H{} {y}"/><path d="M{x} {}V{}"/></g>"#, x - 1.5, x + 1.5, y - 1.5, y + 1.5);
    }
    format!(
        r##"<svg data-stage-art="nightly" class="stage-art" fill="none" preserveAspectRatio="xMinYMin slice" viewBox="{view_box}" xmlns="http://www.w3.org/2000/svg"><defs>
<linearGradient id="{sky}" x1="24" y1="0" x2="264" y2="96" gradientUnits="userSpaceOnUse" spreadMethod="reflect"><stop style="stop-color:var(--stage-night-bottom)"/><stop offset="0.5" style="stop-color:var(--stage-night-mid)"/><stop offset="1" style="stop-color:var(--stage-night-top)"/></linearGradient>
<radialGradient id="{glow}" cx="0" cy="0" r="1" gradientTransform="translate(216 18) rotate(137) scale(120 84)" gradientUnits="userSpaceOnUse"><stop style="stop-color:var(--stage-night-glow-highlight)" stop-opacity="0.4"/><stop offset="0.5" style="stop-color:var(--stage-night-glow-secondary)" stop-opacity="0.16"/><stop offset="1" style="stop-color:var(--stage-night-bottom)" stop-opacity="0"/></radialGradient>
<linearGradient id="{cloud}" x1="0" y1="60" x2="288" y2="96" gradientUnits="userSpaceOnUse"><stop style="stop-color:var(--stage-night-highlight)" stop-opacity="0.5"/><stop offset="0.52" style="stop-color:var(--stage-night-secondary)" stop-opacity="0.62"/><stop offset="1" style="stop-color:var(--stage-night-tertiary)" stop-opacity="0.5"/></linearGradient>
<filter id="{soft}" x="-24" y="-24" width="336" height="144" filterUnits="userSpaceOnUse"><feGaussianBlur stdDeviation="4"/></filter>
<pattern id="{stars}" width="288" height="96" patternUnits="userSpaceOnUse"><g style="fill:var(--stage-night-line)">{stars_markup}</g><g style="stroke:var(--stage-night-sparkle)" stroke-linecap="round" stroke-opacity="0.7" stroke-width="0.6">{sparkles}</g></pattern>
<pattern id="{glows}" width="640" height="96" patternUnits="userSpaceOnUse"><rect width="640" height="96" fill="url(#{glow})"/></pattern>
</defs>
<rect width="100%" height="96" fill="url(#{sky})"/><rect width="100%" height="96" fill="url(#{glows})"/><rect width="100%" height="96" fill="url(#{stars})"/>
<g filter="url(#{soft})"><path d="M-12 88C-12 74 0 63 14 63C18 50 30 41 44 41C58 41 70 49 74 62C79 57 86 54 94 54C110 54 123 66 124 82C132 83 138 88 141 96H-12V88Z" fill="url(#{cloud})"/></g>
<g filter="url(#{soft})"><path d="M150 96C151 84 161 75 173 75C176 64 186 57 198 57C210 57 220 64 223 75C231 75 238 80 241 87C250 87 257 91 260 96H150Z" fill="url(#{cloud})" fill-opacity="0.8"/></g></svg>"##
    )
}

/// The Dev blueprint.
fn blueprint(id: &str, compact: bool) -> String {
    let view_box = if compact { "64 0 8192 96" } else { BACKDROP_VIEW_BOX };
    let (paper, glow, celeste, violet, minor, major, ruler, glows, notes) = (
        format!("{id}-stage-bp-paper"),
        format!("{id}-stage-bp-glow"),
        format!("{id}-stage-bp-glow-celeste"),
        format!("{id}-stage-bp-glow-violet"),
        format!("{id}-stage-bp-grid-minor"),
        format!("{id}-stage-bp-grid-major"),
        format!("{id}-stage-bp-ruler"),
        format!("{id}-stage-bp-glows"),
        format!("{id}-stage-bp-annotations"),
    );
    format!(
        r##"<svg data-stage-art="blueprint" class="stage-art" fill="none" preserveAspectRatio="xMinYMin slice" viewBox="{view_box}" xmlns="http://www.w3.org/2000/svg"><defs>
<linearGradient id="{paper}" x1="60" y1="0" x2="220" y2="96" gradientUnits="userSpaceOnUse" spreadMethod="reflect"><stop style="stop-color:var(--stage-art-bottom)"/><stop offset="0.5" style="stop-color:var(--stage-art-mid)"/><stop offset="1" style="stop-color:var(--stage-art-top)"/></linearGradient>
<radialGradient id="{glow}" cx="0" cy="0" r="1" gradientTransform="translate(216 14) rotate(137) scale(120 84)" gradientUnits="userSpaceOnUse"><stop style="stop-color:var(--stage-art-highlight)" stop-opacity="0.4"/><stop offset="0.52" style="stop-color:var(--stage-art-secondary)" stop-opacity="0.16"/><stop offset="1" style="stop-color:var(--stage-art-bottom)" stop-opacity="0"/></radialGradient>
<radialGradient id="{celeste}" cx="0" cy="0" r="1" gradientTransform="translate(474 44) rotate(166) scale(156 92)" gradientUnits="userSpaceOnUse"><stop style="stop-color:var(--stage-art-celeste-highlight)" stop-opacity="0.34"/><stop offset="0.5" style="stop-color:var(--stage-art-celeste-secondary)" stop-opacity="0.18"/><stop offset="1" style="stop-color:var(--stage-art-bottom)" stop-opacity="0"/></radialGradient>
<radialGradient id="{violet}" cx="0" cy="0" r="1" gradientTransform="translate(704 18) rotate(145) scale(132 88)" gradientUnits="userSpaceOnUse"><stop style="stop-color:var(--stage-art-violet-highlight)" stop-opacity="0.3"/><stop offset="0.52" style="stop-color:var(--stage-art-tertiary)" stop-opacity="0.14"/><stop offset="1" style="stop-color:var(--stage-art-bottom)" stop-opacity="0"/></radialGradient>
<pattern id="{minor}" width="8" height="8" patternUnits="userSpaceOnUse"><path d="M8 0H0V8" style="stroke:var(--stage-art-grid-line)" stroke-opacity="0.14" stroke-width="0.5"/></pattern>
<pattern id="{major}" width="32" height="32" patternUnits="userSpaceOnUse"><path d="M32 0H0V32" style="stroke:var(--stage-art-grid-line)" stroke-opacity="0.26" stroke-width="0.6"/></pattern>
<pattern id="{ruler}" width="32" height="6" patternUnits="userSpaceOnUse"><path d="M4 0V2.5M12 0V2.5M20 0V4M28 0V2.5" style="stroke:var(--stage-art-line)" stroke-opacity="0.5" stroke-width="0.5"/></pattern>
<pattern id="{glows}" width="768" height="96" patternUnits="userSpaceOnUse"><rect width="768" height="96" fill="url(#{glow})"/><rect width="768" height="96" fill="url(#{celeste})"/><rect width="768" height="96" fill="url(#{violet})"/></pattern>
<pattern id="{notes}" width="768" height="96" patternUnits="userSpaceOnUse">
<g style="stroke:var(--stage-art-line)" stroke-linecap="round" stroke-opacity="0.6" stroke-width="0.7"><path d="M180 64H264" stroke-dasharray="5 4"/><path d="M180 61V67M264 61V67"/><path d="M276 10V44" stroke-dasharray="4 4" stroke-opacity="0.5"/><path d="M273 10H279M273 44H279" stroke-opacity="0.5"/><path d="M348 30H428" stroke-dasharray="3.5 5" stroke-opacity="0.5"/><path d="M348 27V33M428 27V33" stroke-opacity="0.5"/><path d="M512 48V80" stroke-dasharray="5 3" stroke-opacity="0.45"/><path d="M509 48H515M509 80H515" stroke-opacity="0.45"/><path d="M590 70H724" stroke-dasharray="7 4" stroke-opacity="0.55"/><path d="M590 67V73M724 67V73" stroke-opacity="0.55"/></g>
<g style="stroke:var(--stage-art-line)" stroke-linecap="round" stroke-opacity="0.55" stroke-width="0.6"><path d="M34 60L38 64M38 60L34 64"/><path d="M228 26H234M231 23V29"/><path d="M143 51H149M146 48V54"/><path d="M316 16L322 22M322 16L316 22"/><path d="M468 70H476M472 66V74"/><path d="M558 28L564 34M564 28L558 34"/><path d="M742 44H750M746 40V48"/></g>
<g style="stroke:var(--stage-art-line)" stroke-opacity="0.35" stroke-width="0.6"><circle cx="196" cy="38" r="13" stroke-dasharray="3.5 4"/><path d="M196 33V43M191 38H201" stroke-opacity="0.6" stroke-width="0.4"/><circle cx="414" cy="64" r="10" stroke-dasharray="2.5 3.5"/><path d="M414 60V68M410 64H418" stroke-opacity="0.6" stroke-width="0.4"/><circle cx="648" cy="32" r="15" stroke-dasharray="4 5"/><path d="M648 26V38M642 32H654" stroke-opacity="0.6" stroke-width="0.4"/></g></pattern>
</defs>
<rect width="100%" height="96" fill="url(#{paper})"/><rect width="100%" height="96" fill="url(#{glows})"/><rect width="100%" height="96" fill="url(#{minor})"/><rect width="100%" height="96" fill="url(#{major})"/><rect width="100%" height="6" fill="url(#{ruler})"/><rect width="100%" height="96" fill="url(#{notes})"/></svg>"##
    )
}

/// One drawing of a channel's art; `compact` is the crop a round button shows.
pub(crate) fn art(variant: Variant, id: &str, compact: bool) -> String {
    match variant {
        Variant::Nightly => nightly(id, compact),
        Variant::Dev => blueprint(id, compact),
    }
}

/// The channel label of the environment: a nightly primary server is "Nightly" whatever this app is.
fn label(state: Store<UiModel>) -> String {
    let version = state.typed_config().read().as_ref().map(|config| config.environment.server_version.to_string());
    stage::stage_label(version.as_deref(), stage::app_stage_label())
}

/// What the Environment identification preference resolves to now (`useEnvironmentIdentificationMode`).
pub(crate) fn mode(state: Store<UiModel>) -> Mode {
    let hydrated = try_consume_context::<crate::client_settings::Writer>().is_none_or(|writer| writer.document.read().hydrated());
    let (active, allows) = try_consume_context::<crate::themes::Themes>()
        .map(|themes| {
            let snapshot = themes.snapshot.read();
            let catalog = themes.catalog.read();
            let half = catalog.half(&snapshot.theme, snapshot.theme_halves.as_ref(), snapshot.resolved_theme);
            (catalog.definition(half).is_some(), catalog.allows_sidebar_artwork(half))
        })
        .unwrap_or((false, false));
    let chosen = state.client_settings().read().environment_identification_mode;
    stage::resolve_mode(chosen, hydrated, active, allows)
}

/// The artwork behind the controls of a channel, when the preference asks for artwork (`useSidebarStageBackdropVariant`).
pub(crate) fn artwork(state: Store<UiModel>) -> Option<Variant> {
    stage::backdrop_variant(&label(state), mode(state) == Mode::Artwork)
}

/// Whether the Environment identification row is offered: only a channel that has a mark (`showEnvironmentIdentification`).
pub(crate) fn offered(state: Store<UiModel>) -> bool {
    stage::pill_label(&label(state)).is_some()
}

/// Gives each drawing of the art its own gradient ids.
fn next_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("sa{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The art across the top of the sidebar, under its brand, faded into the sidebar below.
#[component]
pub(crate) fn Backdrop(variant: Variant) -> Element {
    let svg = use_hook(|| art(variant, &next_id(), false));
    rsx! { div { class: "sidebar-stage-backdrop", "aria-hidden": "true", dangerous_inner_html: "{svg}" } }
}

/// The art cropped to a round button (the send button).
#[component]
pub(crate) fn ButtonArt(variant: Variant) -> Element {
    let svg = use_hook(|| art(variant, &next_id(), true));
    rsx! { span { class: "stage-button-art", "aria-hidden": "true", dangerous_inner_html: "{svg}" } }
}

/// The version pill beside the brand, when the preference asks for it and the channel has one (`data-environment-identification`).
#[component]
pub(crate) fn Pill(state: Store<UiModel>) -> Element {
    let text = if mode(state) == Mode::Pill { stage::pill_label(&label(state)) } else { None };
    rsx! { if let Some(text) = text { span { class: "stage-pill", "data-environment-identification": "pill", "aria-label": "{text} environment", "{text}" } } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reference_in_a_drawing_names_a_definition_of_the_same_drawing() {
        for variant in [Variant::Nightly, Variant::Dev] {
            for compact in [false, true] {
                let svg = art(variant, "x", compact);
                let defined: Vec<&str> = svg.split(" id=\"").skip(1).map(|rest| rest.split('"').next().unwrap()).collect();
                assert!(!defined.is_empty());
                for reference in svg.split("url(#").skip(1) {
                    let name = reference.split(')').next().unwrap();
                    assert!(defined.contains(&name), "{name} is not defined");
                }
                assert!(defined.iter().all(|id| id.starts_with("x-stage-")));
                let mut sorted = defined.clone();
                sorted.sort_unstable();
                sorted.dedup();
                assert_eq!(sorted.len(), defined.len(), "an id is defined twice");
            }
        }
    }

    #[test]
    fn the_compact_crop_shifts_the_view_box_and_the_two_channels_differ() {
        assert!(art(Variant::Nightly, "a", true).contains(r#"viewBox="96 0 8192 96""#));
        assert!(art(Variant::Dev, "a", true).contains(r#"viewBox="64 0 8192 96""#));
        assert!(art(Variant::Dev, "a", false).contains(r#"viewBox="0 0 8192 96""#));
        assert!(art(Variant::Nightly, "a", false).contains(r#"data-stage-art="nightly""#));
        assert!(art(Variant::Dev, "a", false).contains(r#"data-stage-art="blueprint""#));
    }
}
