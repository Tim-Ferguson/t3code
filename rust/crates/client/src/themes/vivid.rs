//! Source themePalette.ts perceptual palette generation. Keep application and Culori matrices distinct.
use super::{
    Appearance, Catalog, Colors,
    color::{self, Oklch},
    library,
};
type Rgb = [f64; 3];
fn mix(base: Rgb, overlay: Rgb, amount: f64) -> Rgb {
    std::array::from_fn(|i| base[i] + (overlay[i] - base[i]) * amount)
}
fn luminance(rgb: Rgb) -> f64 {
    let linear = rgb.map(|v| {
        let c = v / 255.;
        if c <= 0.03928 {
            c / 12.92
        } else {
            libm::pow((c + 0.055) / 1.055, 2.4)
        }
    });
    0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]
}
fn contrast(a: Rgb, b: Rgb) -> f64 {
    let a = luminance(a);
    let b = luminance(b);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}
fn oklch(rgb: Rgb) -> Oklch {
    let [r, g, b] = rgb.map(|v| {
        let c = v / 255.;
        if c <= 0.04045 {
            c / 12.92
        } else {
            libm::pow((c + 0.055) / 1.055, 2.4)
        }
    });
    let l = color::cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
    let m = color::cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
    let s = color::cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
    let a = 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s;
    let b = 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s;
    Oklch {
        l: 0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
        c: a.hypot(b),
        h: b.atan2(a) * 180. / std::f64::consts::PI,
        alpha: 1.,
    }
}
fn from_rgb(rgb: Rgb) -> String {
    color::format(&oklch(rgb))
}
fn mapped(c: &Oklch) -> String {
    color::format(&Oklch {
        alpha: 1.,
        ..color::gamut(c)
    })
}
fn parse(value: &str, fallback: Rgb) -> Rgb {
    color::parse(value)
        .map(|c| color::rgb(&c))
        .unwrap_or(fallback)
}
fn solve(base: &Oklch, against: Rgb, min: f64, lighter: bool) -> Oklch {
    let mut low = if lighter { base.l } else { 0. };
    let mut high = if lighter { 1. } else { base.l };
    if contrast(color::rgb(base), against) >= min {
        return base.clone();
    }
    for _ in 0..18 {
        let mid = (low + high) / 2.;
        let c = Oklch {
            l: mid,
            ..base.clone()
        };
        if contrast(color::rgb(&c), against) >= min {
            if lighter { high = mid } else { low = mid }
        } else if lighter {
            low = mid
        } else {
            high = mid
        }
    }
    Oklch {
        l: if lighter { high } else { low },
        ..base.clone()
    }
}
fn readable(background: Rgb) -> Rgb {
    let light = [255., 250., 255.];
    let dark = [36., 21., 35.];
    let lc = contrast(background, light);
    let dc = contrast(background, dark);
    if lc.max(dc) >= 4.5 {
        if lc >= dc { light } else { dark }
    } else if contrast(background, [255.; 3]) >= contrast(background, [0.; 3]) {
        [255.; 3]
    } else {
        [0.; 3]
    }
}
fn text(background: Rgb, foreground: Rgb, min: f64) -> Rgb {
    if contrast(background, background) >= min {
        return background;
    }
    let mut result = foreground;
    let mut low = 0.;
    let mut high = 1.;
    for _ in 0..12 {
        let mid = (low + high) / 2.;
        let candidate = mix(foreground, background, mid);
        if contrast(candidate, background) >= min {
            result = candidate;
            low = mid
        } else {
            high = mid
        }
    }
    result
}
fn muted(background: Rgb, foreground: Rgb) -> Rgb {
    text(
        background,
        foreground,
        if luminance(background) < 0.179 {
            5.082
        } else {
            4.705
        },
    )
}
fn status(canvas: Rgb) -> Colors {
    let dark = luminance(canvas) < 0.179;
    let mut colors = Colors::new();
    for (role, base, fore) in if dark {
        [
            ("error", "#fb414a", "#ff6467"),
            ("warning", "#fe9a00", "#ffb900"),
        ]
    } else {
        [
            ("error", "#fb2c36", "#c10007"),
            ("warning", "#fe9a00", "#bb4d00"),
        ]
    } {
        let surface = mix(canvas, parse(base, canvas), if dark { 0.16 } else { 0.08 });
        colors.insert(role.into(), color::canonical(base).unwrap());
        colors.insert(
            format!("{role}Foreground"),
            mapped(&solve(&oklch(parse(fore, canvas)), surface, 4.6, dark)),
        );
        colors.insert(format!("{role}Surface"), from_rgb(surface));
    }
    colors
}
pub fn create(catalog: &Catalog, appearance: Appearance, background: &str, accent: &str) -> Colors {
    let canvas_rgb = parse(
        background,
        if appearance == Appearance::Dark {
            [24., 15., 27.]
        } else {
            [250., 245., 250.]
        },
    );
    let accent_rgb = parse(accent, [168., 67., 112.]);
    let canvas = oklch(canvas_rgb);
    let accent = oklch(accent_rgb);
    let dark = luminance(canvas_rgb) < 0.179;
    let hue = if accent.c < 0.02 { canvas.h } else { accent.h };
    let tint = (accent.c * 0.22).max(0.008).min(0.045);
    let surface = |delta: f64, c: f64| Oklch {
        l: (canvas.l + if dark { delta } else { -delta })
            .max(0.05)
            .min(0.98),
        c,
        h: hue,
        alpha: 1.,
    };
    let text_base = Oklch {
        l: if dark { 0.95 } else { 0.2 },
        c: (accent.c * 0.25).min(0.035),
        h: hue,
        alpha: 1.,
    };
    let text_rgb = color::rgb(&solve(&text_base, canvas_rgb, 7., dark));
    let text_muted = muted(canvas_rgb, text_rgb);
    let action = Oklch {
        l: (accent.l + if dark { 0.06 } else { -0.02 })
            .max(0.35)
            .min(0.85),
        c: (accent.c * 0.9).max(0.06),
        h: (hue + 50.) % 360.,
        alpha: 1.,
    };
    let action_rgb = color::rgb(&action);
    let mut colors = library::defaults(catalog, appearance);
    colors.extend(status(canvas_rgb));
    let mut set = |role: &str, value: String| {
        colors.insert(role.into(), value);
    };
    for role in ["canvas", "chrome", "toolbar", "terminalBackground"] {
        set(role, from_rgb(canvas_rgb));
    }
    for role in [
        "toolbarForeground",
        "toolbarControlForeground",
        "text",
        "codeForeground",
        "terminalForeground",
    ] {
        set(role, from_rgb(text_rgb));
    }
    for role in ["textMuted", "secondaryLabel", "iconMuted"] {
        set(role, from_rgb(text_muted));
    }
    for role in ["focus", "accent", "update", "terminalCursor"] {
        set(role, from_rgb(accent_rgb));
    }
    set("accentForeground", from_rgb(readable(accent_rgb)));
    for (role, delta, chroma) in [
        (
            "toolbarBorder",
            if dark { 0.14 } else { 0.1 },
            (accent.c * 0.4).min(0.08),
        ),
        ("toolbarControl", if dark { 0.09 } else { 0.05 }, tint * 1.3),
        (
            "toolbarControlHover",
            if dark { 0.14 } else { 0.09 },
            tint * 1.6,
        ),
        ("surface", 0.015, tint),
        ("surfaceRaised", 0.05, tint),
        ("surfaceOverlay", 0.075, tint),
        (
            "border",
            if dark { 0.16 } else { 0.12 },
            (accent.c * 0.35).min(0.07),
        ),
        (
            "input",
            if dark { 0.21 } else { 0.16 },
            (accent.c * 0.4).min(0.08),
        ),
        (
            "secondary",
            if dark { 0.1 } else { 0.06 },
            (accent.c * 0.5).min(0.09),
        ),
        (
            "muted",
            if dark { 0.06 } else { 0.04 },
            (accent.c * 0.35).min(0.06),
        ),
        (
            "updateSurface",
            if dark { 0.14 } else { 0.09 },
            (accent.c * 0.55).min(0.12),
        ),
        (
            "accentSurface",
            if dark { 0.13 } else { 0.08 },
            (accent.c * 0.55).min(0.11),
        ),
        (
            "messageSurface",
            if dark { 0.16 } else { 0.1 },
            (accent.c * 0.6).min(0.13),
        ),
        ("codeBackground", 0.035, tint * 0.8),
        ("sidebar", 0.045, tint * 1.4),
        (
            "sidebarControlSurface",
            if dark { 0.1 } else { 0.07 },
            tint * 1.5,
        ),
        (
            "sidebarRowHover",
            if dark { 0.08 } else { 0.06 },
            (accent.c * 0.45).min(0.08),
        ),
        (
            "sidebarRowActive",
            if dark { 0.12 } else { 0.09 },
            (accent.c * 0.55).min(0.1),
        ),
        (
            "sidebarRowSelected",
            if dark { 0.14 } else { 0.1 },
            (accent.c * 0.6).min(0.11),
        ),
        (
            "sidebarBorder",
            if dark { 0.17 } else { 0.12 },
            (accent.c * 0.4).min(0.08),
        ),
        (
            "terminalSelection",
            if dark { 0.18 } else { 0.12 },
            (accent.c * 0.55).min(0.12),
        ),
        ("terminalScrollbar", if dark { 0.22 } else { 0.16 }, tint),
        (
            "terminalScrollbarHover",
            if dark { 0.3 } else { 0.22 },
            tint,
        ),
    ] {
        set(role, mapped(&surface(delta, chroma)));
    }
    for (role, delta, chroma) in [
        (
            "secondaryForeground",
            if dark { 0.1 } else { 0.06 },
            (accent.c * 0.5).min(0.09),
        ),
        (
            "updateForeground",
            if dark { 0.14 } else { 0.09 },
            (accent.c * 0.55).min(0.12),
        ),
        (
            "accentSurfaceForeground",
            if dark { 0.13 } else { 0.08 },
            (accent.c * 0.55).min(0.11),
        ),
        (
            "messageForeground",
            if dark { 0.16 } else { 0.1 },
            (accent.c * 0.6).min(0.13),
        ),
        ("sidebarForeground", 0.045, tint * 1.4),
    ] {
        set(
            role,
            mapped(&solve(
                &text_base,
                color::rgb(&surface(delta, chroma)),
                4.6,
                dark,
            )),
        );
    }
    set(
        "mutedForeground",
        from_rgb(text(
            color::rgb(&surface(
                if dark { 0.06 } else { 0.04 },
                (accent.c * 0.35).min(0.06),
            )),
            text_rgb,
            4.6,
        )),
    );
    set(
        "placeholder",
        from_rgb(text(color::rgb(&surface(0.05, tint)), text_rgb, 4.6)),
    );
    set("messageAction", from_rgb(action_rgb));
    set("messageActionForeground", from_rgb(readable(action_rgb)));
    set(
        "messageActionHover",
        mapped(&Oklch {
            l: action.l + if dark { 0.06 } else { -0.06 },
            ..action
        }),
    );
    set(
        "sidebarMutedForeground",
        from_rgb(muted(color::rgb(&surface(0.045, tint * 1.4)), text_rgb)),
    );
    colors
}
/// Advanced-editor family edits preserve all unrelated imported roles.
pub fn update(appearance: Appearance, colors: &Colors, role: &str, value: &str) -> Colors {
    let mut next = colors.clone();
    let Some(selected) = color::parse(value) else {
        next.insert(role.into(), value.into());
        return next;
    };
    let normalized = color::format(&selected);
    let canvas = parse(
        colors.get("canvas").map(String::as_str).unwrap_or(""),
        if appearance == Appearance::Dark {
            [24., 15., 27.]
        } else {
            [250., 245., 250.]
        },
    );
    let rgb = color::rgb(&selected);
    let on = mix(canvas, rgb, selected.alpha);
    let accent = parse(
        colors.get("accent").map(String::as_str).unwrap_or(""),
        [168., 67., 112.],
    );
    let dark = luminance(canvas) < 0.179;
    let fore = |bg| from_rgb(readable(bg));
    let tone = |bg| mapped(&solve(&selected, bg, 4.6, luminance(bg) < 0.179));
    let mut changes: Vec<(&str, String)> = vec![(role, normalized.clone())];
    match role {
        "canvas" => changes.extend([("chrome", normalized.clone()), ("toolbar", normalized)]),
        "text" => changes.extend([
            ("toolbarForeground", normalized.clone()),
            ("toolbarControlForeground", normalized),
        ]),
        "mutedForeground" => {
            for role in [
                "textMuted",
                "placeholder",
                "secondaryLabel",
                "iconMuted",
                "sidebarMutedForeground",
            ] {
                changes.push((role, normalized.clone()))
            }
        }
        "border" => changes.extend([
            ("toolbarBorder", normalized.clone()),
            ("sidebarBorder", normalized),
        ]),
        "secondary" => changes.extend([
            ("secondaryForeground", fore(on)),
            ("muted", normalized.clone()),
            ("toolbarControl", normalized),
        ]),
        "accentSurface" => changes.extend([
            ("accentSurfaceForeground", fore(on)),
            ("toolbarControlHover", normalized),
        ]),
        "accent" => {
            let surface = mix(canvas, on, if dark { 0.32 } else { 0.16 });
            changes.extend([
                ("accentForeground", fore(on)),
                ("focus", normalized.clone()),
                ("update", normalized.clone()),
                ("terminalCursor", normalized),
                ("updateForeground", tone(surface)),
                ("updateSurface", from_rgb(surface)),
            ]);
        }
        "messageAction" => {
            let fore = readable(on);
            let opposite = if fore == [255., 250., 255.] || fore == [255.; 3] {
                [0.; 3]
            } else {
                [255.; 3]
            };
            let mut hover = oklch(mix(rgb, opposite, 0.12));
            hover.alpha = selected.alpha;
            changes.extend([
                ("messageActionForeground", from_rgb(fore)),
                ("messageActionHover", color::format(&hover)),
            ]);
        }
        "messageSurface" => changes.push(("messageForeground", fore(on))),
        "codeBackground" => changes.push(("codeForeground", fore(on))),
        "sidebar" => changes.push(("sidebarForeground", fore(on))),
        "sidebarRowSelected" => {
            let sidebar = parse(
                colors.get("sidebar").map(String::as_str).unwrap_or(""),
                canvas,
            );
            let on = mix(sidebar, rgb, selected.alpha);
            changes.extend([
                ("sidebarRowHover", from_rgb(mix(sidebar, on, 0.5))),
                ("sidebarRowActive", from_rgb(mix(sidebar, on, 0.8))),
            ]);
        }
        "terminalBackground" => {
            let fore = readable(on);
            let dark = luminance(on) < 0.179;
            changes.extend([
                ("terminalForeground", from_rgb(fore)),
                (
                    "terminalSelection",
                    from_rgb(mix(on, accent, if dark { 0.35 } else { 0.18 })),
                ),
                (
                    "terminalScrollbar",
                    from_rgb(mix(on, fore, if dark { 0.42 } else { 0.22 })),
                ),
                (
                    "terminalScrollbarHover",
                    from_rgb(mix(on, fore, if dark { 0.55 } else { 0.32 })),
                ),
            ]);
        }
        "error" | "warning" => {
            let surface = mix(canvas, on, if dark { 0.16 } else { 0.08 });
            if role == "error" {
                changes.extend([
                    ("errorForeground", tone(surface)),
                    ("errorSurface", from_rgb(surface)),
                ]);
            } else {
                changes.extend([
                    ("warningForeground", tone(surface)),
                    ("warningSurface", from_rgb(surface)),
                ]);
            }
        }
        _ => {}
    }
    for (role, value) in changes {
        next.insert(role.into(), value);
    }
    next
}
