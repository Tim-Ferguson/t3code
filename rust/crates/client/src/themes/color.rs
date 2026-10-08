//! CSS literal parsing and f64 conversion derived from Culori4.0.2 (MIT).
//! See rust/vendor/culori/LICENSE. App canonicalization follows original themePalette.ts.
use regex::Regex;
use std::{collections::BTreeMap, sync::LazyLock};
const NUMBER: &str = r"[+-]?\d*\.?\d+(?:[eE][+-]?\d+)?";
static NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("^{NUMBER}")).unwrap());
static ALPHA_NONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)/[\t-\r \x{00a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]*none[\t-\r \x{00a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]*\)$").unwrap()
});
static NAMED: LazyLock<BTreeMap<String, u32>> =
    LazyLock::new(|| serde_json::from_str(include_str!("named-colors.json")).unwrap());
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Number,
    Percent,
    Hue,
    None,
}
#[derive(Clone, Copy, Debug)]
struct Coord {
    kind: Kind,
    value: f64,
}
#[derive(Clone, Debug)]
pub struct Oklch {
    pub l: f64,
    pub c: f64,
    pub h: f64,
    pub alpha: f64,
}
#[derive(Clone, Copy, Debug)]
struct Color<'a> {
    mode: &'a str,
    v: [f64; 3],
    alpha: f64,
}
pub(super) fn trim(value: &str) -> &str {
    value.trim_matches(|c:char|matches!(c,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'))
}
fn ident_start(c: char) -> bool {
    !c.is_ascii() || c.is_ascii_alphabetic() || c == '_'
}
fn ident_char(c: char) -> bool {
    ident_start(c) || c.is_ascii_digit() || c == '-'
}
fn starts_ident(value: &str) -> bool {
    let mut c = value.chars();
    match c.next() {
        Some('-') => c.next().is_some_and(|c| c == '-' || ident_start(c)),
        Some(c) => ident_start(c),
        None => false,
    }
}
fn take_ident(value: &str) -> (&str, &str) {
    let end = value
        .char_indices()
        .find(|(_, c)| !ident_char(*c))
        .map(|(i, _)| i)
        .unwrap_or(value.len());
    value.split_at(end)
}
fn numeric(value: &str) -> Option<(Coord, &str)> {
    let number = NUM.find(value)?.as_str();
    let mut tail = &value[number.len()..];
    let mut n = number.parse::<f64>().ok()?;
    let kind = if starts_ident(tail) {
        let (id, remaining) = take_ident(tail);
        tail = remaining;
        let scale = match id {
            "deg" => 1.,
            "rad" => 180. / std::f64::consts::PI,
            "grad" => 9. / 10.,
            "turn" => 360.,
            _ => return None,
        };
        n *= scale;
        Kind::Hue
    } else if tail.starts_with('%') {
        tail = &tail[1..];
        Kind::Percent
    } else {
        Kind::Number
    };
    Some((Coord { kind, value: n }, tail))
}
fn coords(mut tail: &str) -> Option<([Coord; 3], Coord)> {
    let mut values = Vec::new();
    let mut alpha = None;
    while !tail.is_empty() {
        let ch = tail.chars().next()?;
        match ch {
            '\n' | '\t' | ' ' => {
                tail = tail.trim_start_matches(['\n', '\t', ' ']);
            }
            ')' => {
                if tail.len() != 1 {
                    return None;
                }
                tail = "";
            }
            '/' => {
                if alpha.is_some() {
                    return None;
                }
                tail = tail[1..].trim_start_matches(['\n', '\t', ' ']);
                let (coordinate, remaining) = component(tail)?;
                if coordinate.kind == Kind::Hue {
                    return None;
                }
                alpha = Some(coordinate);
                tail = remaining;
                if !tail.trim_start_matches(['\n', '\t', ' ']).is_empty()
                    && !tail.trim_start_matches(['\n', '\t', ' ']).eq(")")
                {
                    return None;
                }
            }
            _ => {
                if alpha.is_some() {
                    return None;
                }
                let (coordinate, remaining) = component(tail)?;
                values.push(coordinate);
                tail = remaining;
            }
        }
    }
    if values.len() != 3 {
        return None;
    }
    Some((
        [values[0], values[1], values[2]],
        alpha.unwrap_or(Coord {
            kind: Kind::None,
            value: 0.,
        }),
    ))
}
fn component(value: &str) -> Option<(Coord, &str)> {
    if starts_ident(value) {
        let (id, tail) = take_ident(value);
        (id == "none").then_some((
            Coord {
                kind: Kind::None,
                value: 0.,
            },
            tail,
        ))
    } else {
        numeric(value)
    }
}
fn clamp(value: f64, min: f64, max: f64) -> f64 {
    if value.is_nan() {
        f64::NAN
    } else {
        value.max(min).min(max)
    }
}
fn alpha(c: Coord) -> f64 {
    match c.kind {
        Kind::None => 1.,
        Kind::Number => clamp(c.value, 0., 1.),
        Kind::Percent => clamp(c.value / 100., 0., 1.),
        Kind::Hue => f64::NAN,
    }
}
fn modern(input: &str) -> Option<Color<'_>> {
    let (name, tail) = take_ident(input);
    let mut tail = tail.strip_prefix('(')?;
    let profile = if name == "color" {
        tail = tail.trim_start_matches(['\n', '\t', ' ']);
        let (profile, remaining) = take_ident(tail);
        tail = remaining;
        Some(profile)
    } else {
        None
    };
    let (c, a) = coords(tail)?;
    let number = |i: usize, percent: f64| {
        if c[i].kind == Kind::Percent {
            c[i].value * percent / 100.
        } else {
            c[i].value
        }
    };
    let value = if let Some(profile) = profile {
        if c.iter().any(|c| c.kind == Kind::Hue) {
            return None;
        }
        let mode = match profile {
            "srgb" => "rgb",
            "srgb-linear" => "lrgb",
            "display-p3" => "p3",
            "a98-rgb" => "a98",
            "prophoto-rgb" => "prophoto",
            "rec2020" => "rec2020",
            "xyz" | "xyz-d65" => "xyz65",
            "xyz-d50" => "xyz50",
            "--hsv" => "hsv",
            "--lab-d65" => "lab65",
            "--lch-d65" => "lch65",
            _ => return None,
        };
        return Some(Color {
            mode,
            v: [number(0, 1.), number(1, 1.), number(2, 1.)],
            alpha: alpha(a),
        });
    } else {
        match name {
            "rgb" | "rgba" => {
                if c.iter().any(|c| c.kind == Kind::Hue) {
                    return None;
                }
                [
                    if c[0].kind == Kind::Percent {
                        c[0].value / 100.
                    } else {
                        c[0].value / 255.
                    },
                    if c[1].kind == Kind::Percent {
                        c[1].value / 100.
                    } else {
                        c[1].value / 255.
                    },
                    if c[2].kind == Kind::Percent {
                        c[2].value / 100.
                    } else {
                        c[2].value / 255.
                    },
                ]
            }
            "hsl" | "hsla" => {
                if c[0].kind == Kind::Percent || c[1].kind == Kind::Hue || c[2].kind == Kind::Hue {
                    return None;
                }
                [c[0].value, c[1].value / 100., c[2].value / 100.]
            }
            "hwb" => {
                if c[0].kind == Kind::Percent || c[1].kind == Kind::Hue || c[2].kind == Kind::Hue {
                    return None;
                }
                [c[0].value, c[1].value / 100., c[2].value / 100.]
            }
            "oklab" => {
                if c.iter().any(|c| c.kind == Kind::Hue) {
                    return None;
                }
                [clamp(number(0, 1.), 0., 1.), number(1, 0.4), number(2, 0.4)]
            }
            "oklch" => {
                if c[0].kind == Kind::Hue || c[2].kind == Kind::Percent {
                    return None;
                }
                [
                    clamp(number(0, 1.), 0., 1.),
                    clamp(
                        if c[1].kind == Kind::Number {
                            c[1].value
                        } else {
                            c[1].value * 0.4 / 100.
                        },
                        0.,
                        f64::INFINITY,
                    ),
                    c[2].value,
                ]
            }
            "lab" => {
                if c.iter().any(|c| c.kind == Kind::Hue) {
                    return None;
                }
                [
                    clamp(c[0].value, 0., 100.),
                    number(1, 125.),
                    number(2, 125.),
                ]
            }
            "lch" => {
                if c[0].kind == Kind::Hue || c[2].kind == Kind::Percent {
                    return None;
                }
                [
                    clamp(c[0].value, 0., 100.),
                    clamp(
                        if c[1].kind == Kind::Number {
                            c[1].value
                        } else {
                            c[1].value * 150. / 100.
                        },
                        0.,
                        f64::INFINITY,
                    ),
                    c[2].value,
                ]
            }
            _ => return None,
        }
    };
    Some(Color {
        mode: match name {
            "rgba" => "rgb",
            "hsla" => "hsl",
            name => name,
        },
        v: value,
        alpha: alpha(a),
    })
}
fn legacy(input: &str) -> Option<Color<'_>> {
    let (name, tail) = input.split_once('(')?;
    let tail = tail.strip_suffix(')')?;
    let values: Vec<_> = tail.split(',').map(trim).collect();
    if !(3..=4).contains(&values.len()) {
        return None;
    }
    let parse = |value: &str| {
        let (coordinate, tail) = numeric(value)?;
        tail.is_empty().then_some(coordinate)
    };
    let c = [parse(values[0])?, parse(values[1])?, parse(values[2])?];
    let a = if values.len() == 4 {
        alpha(parse(values[3])?)
    } else {
        1.
    };
    match name {
        "rgb" | "rgba" => {
            let percent = c[0].kind == Kind::Percent;
            if c.iter()
                .any(|c| c.kind != if percent { Kind::Percent } else { Kind::Number })
            {
                return None;
            }
            let scale = if percent { 100. } else { 255. };
            Some(Color {
                mode: "rgb",
                v: c.map(|c| c.value / scale),
                alpha: a,
            })
        }
        "hsl" | "hsla" => {
            if !matches!(c[0].kind, Kind::Number | Kind::Hue)
                || c[1].kind != Kind::Percent
                || c[2].kind != Kind::Percent
            {
                return None;
            }
            Some(Color {
                mode: "hsl",
                v: [
                    c[0].value,
                    clamp(c[1].value / 100., 0., 1.),
                    clamp(c[2].value / 100., 0., 1.),
                ],
                alpha: a,
            })
        }
        _ => None,
    }
}
fn hexadecimal(input: &str) -> Option<Color<'static>> {
    let hex = input.strip_prefix('#').unwrap_or(input);
    if !matches!(hex.len(), 3 | 4 | 6 | 8) || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let number = u32::from_str_radix(hex, 16).ok()?;
    let (r, g, b, a) = match hex.len() {
        3 => (
            (number >> 8) * 17,
            (number >> 4 & 15) * 17,
            (number & 15) * 17,
            255,
        ),
        4 => (
            (number >> 12) * 17,
            (number >> 8 & 15) * 17,
            (number >> 4 & 15) * 17,
            (number & 15) * 17,
        ),
        6 => (number >> 16, number >> 8 & 255, number & 255, 255),
        8 => (
            number >> 24,
            number >> 16 & 255,
            number >> 8 & 255,
            number & 255,
        ),
        _ => unreachable!(),
    };
    Some(Color {
        mode: "rgb",
        v: [r as f64 / 255., g as f64 / 255., b as f64 / 255.],
        alpha: a as f64 / 255.,
    })
}
pub fn parse(input: &str) -> Option<Oklch> {
    let input = trim(input);
    let color = modern(input)
        .or_else(|| hexadecimal(input))
        .or_else(|| {
            NAMED
                .get(&input.to_lowercase())
                .and_then(|n| hexadecimal(&format!("#{n:06x}")))
        })
        .or_else(|| {
            (input == "transparent").then_some(Color {
                mode: "rgb",
                v: [0.; 3],
                alpha: 0.,
            })
        })
        .or_else(|| legacy(input))?;
    let mut v = match color.mode {
        "oklch" => color.v,
        "oklab" => lab_to_lch(color.v),
        _ => lab_to_lch(rgb_to_oklab(to_rgb(color)?)),
    };
    v[0] = clamp(v[0], 0., 1.);
    v[1] = clamp(v[1], 0., f64::INFINITY);
    let alpha = if ALPHA_NONE.is_match(input) {
        0.
    } else {
        color.alpha
    };
    [v[0], v[1], v[2], alpha]
        .into_iter()
        .all(f64::is_finite)
        .then_some(Oklch {
            l: v[0],
            c: v[1],
            h: v[2],
            alpha: clamp(alpha, 0., 1.),
        })
}
fn hue(value: f64) -> f64 {
    ((value % 360.) + 360.) % 360.
}
fn lab_to_lch([l, a, b]: [f64; 3]) -> [f64; 3] {
    let c = (a * a + b * b).sqrt();
    [
        l,
        c,
        if c != 0. {
            hue(b.atan2(a) * 180. / std::f64::consts::PI)
        } else {
            0.
        },
    ]
}
fn lch_to_lab([l, c, h]: [f64; 3]) -> [f64; 3] {
    [
        l,
        c * (h / 180. * std::f64::consts::PI).cos(),
        c * (h / 180. * std::f64::consts::PI).sin(),
    ]
}
fn linear(c: f64) -> f64 {
    if c.abs() <= 0.04045 {
        c / 12.92
    } else {
        c.signum() * libm::pow((c.abs() + 0.055) / 1.055, 2.4)
    }
}
fn encoded(c: f64) -> f64 {
    if c.abs() > 0.0031308 {
        c.signum() * (1.055 * libm::pow(c.abs(), 1. / 2.4) - 0.055)
    } else {
        c * 12.92
    }
}
fn matrix(v: [f64; 3], m: [[f64; 3]; 3]) -> [f64; 3] {
    m.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2])
}
fn rgb_to_oklab(rgb: [f64; 3]) -> [f64; 3] {
    let lms = matrix(
        rgb.map(linear),
        [
            [0.412221469470763, 0.5363325372617348, 0.0514459932675022],
            [0.2119034958178252, 0.6806995506452344, 0.1073969535369406],
            [0.0883024591900564, 0.2817188391361215, 0.6299787016738222],
        ],
    )
    .map(cbrt);
    let mut lab = matrix(
        lms,
        [
            [0.210454268309314, 0.7936177747023054, -0.0040720430116193],
            [1.9779985324311684, -2.4285922420485799, 0.450593709617411],
            [0.0259040424655478, 0.7827717124575296, -0.8086757549230774],
        ],
    );
    if rgb[0] == rgb[1] && rgb[1] == rgb[2] {
        lab[1] = 0.;
        lab[2] = 0.;
    }
    lab
}
fn xyz_rgb(v: [f64; 3], d50: bool) -> [f64; 3] {
    matrix(
        v,
        if d50 {
            [
                [3.1341359569958707, -1.6173863321612538, -0.4906619460083532],
                [-0.978795502912089, 1.916254567259524, 0.03344273116131949],
                [0.07195537988411677, -0.2289768264158322, 1.405386058324125],
            ]
        } else {
            [
                [3.2409699419045226, -1.5373831775700939, -0.4986107602930034],
                [-0.9692436362808796, 1.8759675015077204, 0.0415550574071756],
                [0.0556300796969936, -0.2039769588889765, 1.0569715142428784],
            ]
        },
    )
    .map(encoded)
}
fn lab_xyz([l, a, b]: [f64; 3], d50: bool) -> [f64; 3] {
    let fy = (l + 16.) / 116.;
    let f = |v: f64| {
        let cube = libm::pow(v, 3.);
        if cube > 216. / 24389. {
            cube
        } else {
            (116. * v - 16.) / (24389. / 27.)
        }
    };
    let white = if d50 {
        [0.3457 / 0.3585, 1., (1. - 0.3457 - 0.3585) / 0.3585]
    } else {
        [0.3127 / 0.329, 1., (1. - 0.3127 - 0.329) / 0.329]
    };
    [
        f(a / 500. + fy) * white[0],
        f(fy),
        f(fy - b / 200.) * white[2],
    ]
}
fn hsv([h, s, v]: [f64; 3]) -> [f64; 3] {
    let h = hue(h);
    let f = ((h / 60.) % 2. - 1.).abs();
    let low = v * (1. - s);
    let middle = v * (1. - s * f);
    match if h.is_finite() {
        (h / 60.).floor() as i32
    } else {
        -1
    } {
        0 => [v, middle, low],
        1 => [middle, v, low],
        2 => [low, v, middle],
        3 => [low, middle, v],
        4 => [middle, low, v],
        5 => [v, low, middle],
        _ => [low; 3],
    }
}
fn to_rgb(color: Color<'_>) -> Option<[f64; 3]> {
    let v = color.v;
    Some(match color.mode {
        "rgb" => v,
        "lrgb" => v.map(encoded),
        "hsv" => hsv(v),
        "hsl" => {
            let [h, s, l] = v;
            let h = hue(h);
            let m1 = l + s * if l < 0.5 { l } else { 1. - l };
            let m2 = m1 - (m1 - l) * 2. * ((h / 60.) % 2. - 1.).abs();
            let low = 2. * l - m1;
            match if h.is_finite() {
                (h / 60.).floor() as i32
            } else {
                -1
            } {
                0 => [m1, m2, low],
                1 => [m2, m1, low],
                2 => [low, m1, m2],
                3 => [low, m2, m1],
                4 => [m2, low, m1],
                5 => [m1, low, m2],
                _ => [low; 3],
            }
        }
        "hwb" => {
            let [h, mut w, mut b] = v;
            if w + b > 1. {
                let s = w + b;
                w /= s;
                b /= s;
            }
            hsv([h, if b == 1. { 1. } else { 1. - w / (1. - b) }, 1. - b])
        }
        "xyz50" => xyz_rgb(v, true),
        "xyz65" => xyz_rgb(v, false),
        "lab" | "lab65" => xyz_rgb(lab_xyz(v, color.mode == "lab"), color.mode == "lab"),
        "lch" | "lch65" => xyz_rgb(
            lab_xyz(lch_to_lab(v), color.mode == "lch"),
            color.mode == "lch",
        ),
        "p3" => xyz_rgb(
            matrix(
                v.map(linear),
                [
                    [0.486570948648216, 0.265667693169093, 0.1982172852343625],
                    [0.2289745640697487, 0.6917385218365062, 0.079286914093745],
                    [0., 0.0451133818589026, 1.043944368900976],
                ],
            ),
            false,
        ),
        "a98" => xyz_rgb(
            matrix(
                v.map(|v| libm::pow(v.abs(), 563. / 256.) * v.signum()),
                [
                    [0.5766690429101305, 0.1855582379065463, 0.1882286462349947],
                    [0.297344975250536, 0.6273635662554661, 0.0752914584939979],
                    [0.0270313613864123, 0.0706888525358272, 0.9913375368376386],
                ],
            ),
            false,
        ),
        "prophoto" => xyz_rgb(
            matrix(
                v.map(|v| {
                    if v.abs() >= 16. / 512. {
                        v.signum() * libm::pow(v.abs(), 1.8)
                    } else {
                        v / 16.
                    }
                }),
                [
                    [0.7977666449006423, 0.1351812974005331, 0.0313477341283922],
                    [0.2880748288194013, 0.7118352342418731, 0.0000899369387256],
                    [0., 0., 0.8251046025104602],
                ],
            ),
            true,
        ),
        "rec2020" => xyz_rgb(
            matrix(
                v.map(|v| {
                    if v.abs() < 0.018053968510807 * 4.5 {
                        v / 4.5
                    } else {
                        v.signum()
                            * ((v.abs() + 1.09929682680944 - 1.) / 1.09929682680944).powf(1. / 0.45)
                    }
                }),
                [
                    [0.6369580483012911, 0.1446169035862083, 0.1688809751641721],
                    [0.262700212011267, 0.6779980715188708, 0.059301716469862],
                    [0., 0.0280726930490874, 1.0609850577107909],
                ],
            ),
            false,
        ),
        _ => return None,
    })
}
fn fixed(value: f64, precision: u32) -> String {
    let value = if value.abs() < 10_f64.powi(-(precision as i32)) / 2. {
        0.
    } else {
        value
    };
    if value.abs() >= 1e21 {
        let encoded = serde_json::to_string(&value).unwrap();
        return if let Some((m, e)) = encoded.split_once('e') {
            let m = m.strip_suffix("0.0").unwrap_or(m);
            let e = e.parse::<i32>().unwrap();
            format!("{m}e{}{e}", if e >= 0 { "+" } else { "" })
        } else {
            encoded
        };
    }
    // ECMAScript toFixed rounds exact binary halfway values upward, unlike Rust's even rule.
    let bits = value.abs().to_bits();
    let exponent = ((bits >> 52) & 2047) as i32 - 1023 - 52;
    let mantissa = (bits & ((1 << 52) - 1)) | if bits >> 52 & 2047 != 0 { 1 << 52 } else { 0 };
    let scaled = (mantissa as u128) * 10u128.pow(precision);
    let integer = if exponent >= 0 {
        scaled << exponent
    } else {
        let shift = (-exponent) as u32;
        if shift >= 128 {
            0
        } else {
            let n = scaled >> shift;
            let rem = scaled & ((1u128 << shift) - 1);
            n + u128::from(rem >= (1u128 << (shift - 1)))
        }
    };
    let factor = 10u128.pow(precision);
    let mut encoded = format!(
        "{}{}.{:0width$}",
        if value < 0. { "-" } else { "" },
        integer / factor,
        integer % factor,
        width = precision as usize
    );
    while encoded.ends_with('0') {
        encoded.pop();
    }
    if encoded.ends_with('.') {
        encoded.pop();
    }
    encoded
}
pub fn format(color: &Oklch) -> String {
    let h = if color.c < 0.0000005 {
        0.
    } else {
        hue(color.h)
    };
    let body = format!(
        "{} {} {}",
        fixed(color.l, 6),
        fixed(color.c, 6),
        fixed(h, 3)
    );
    if color.alpha < 1. {
        format!("oklch({body} / {})", fixed(color.alpha, 4))
    } else {
        format!("oklch({body})")
    }
}
pub fn canonical(input: &str) -> Option<String> {
    parse(input).map(|color| format(&color))
}
fn app_linear(color: &Oklch) -> [f64; 3] {
    let hr = color.h * std::f64::consts::PI / 180.;
    let a = color.c * hr.cos();
    let b = color.c * hr.sin();
    let l = (color.l + 0.3963377774 * a + 0.2158037573 * b).powi(3);
    let m = (color.l - 0.1055613458 * a - 0.0638541728 * b).powi(3);
    let s = (color.l - 0.0894841775 * a - 1.291485548 * b).powi(3);
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
    ]
}
pub fn gamut(color: &Oklch) -> Oklch {
    let in_gamut = |c: f64| {
        app_linear(&Oklch { c, ..color.clone() })
            .into_iter()
            .all(|channel| (-0.0001..=1.0001).contains(&channel))
    };
    if in_gamut(color.c) {
        return color.clone();
    }
    let mut low = 0.;
    let mut high = color.c;
    let steps = (color.c.max(0.000001).log2() - 0.000001_f64.log2())
        .ceil()
        .max(1.) as u32;
    for _ in 0..steps {
        let mid = (low + high) / 2.;
        if in_gamut(mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    Oklch {
        c: low,
        ..color.clone()
    }
}
pub fn rgb(color: &Oklch) -> [f64; 3] {
    app_linear(&gamut(color)).map(|channel| {
        let c = if channel <= 0.0031308 {
            channel * 12.92
        } else {
            1.055 * channel.powf(1. / 2.4) - 0.055
        };
        (clamp(c, 0., 1.) * 255.).round()
    })
}
pub fn hex(input: &str) -> Option<String> {
    let color = parse(input)?;
    let mut result = "#".to_owned();
    for v in rgb(&color) {
        result += &format!("{:02x}", v as u8);
    }
    if color.alpha < 1. {
        result += &format!("{:02x}", (color.alpha * 255.).round() as u8);
    }
    Some(result)
}

// Original V8 12.9 fdlibm cbrt, rather than the newer correctly-rounded libm variant.
// BSD-licensed V8 project source: src/base/ieee754.cc (see vendor/v8/LICENSE).
pub(super) fn cbrt(x: f64) -> f64 {
    let bits = x.to_bits();
    let high = (bits >> 32) as u32;
    let sign = high & 0x80000000;
    let hx = high ^ sign;
    if hx >= 0x7ff00000 {
        return x + x;
    }
    if hx == 0 && bits as u32 == 0 {
        return x;
    }
    let estimate = if hx < 0x00100000 {
        let scaled = f64::from_bits(0x43500000_u64 << 32) * x;
        (((scaled.to_bits() >> 32) as u32 & 0x7fffffff) / 3 + 696219795) | sign
    } else {
        (hx / 3 + 715094163) | sign
    };
    let mut t = f64::from_bits((estimate as u64) << 32);
    let r = (t * t) * (t / x);
    t = t
        * ((1.87595182427177009643 + r * (-1.88497979543377169875 + r * 1.621429720105354466140))
            + ((r * r) * r) * (-0.758397934778766047437 + r * 0.145996192886612446982));
    t = f64::from_bits(t.to_bits().wrapping_add(0x80000000) & 0xffffffffc0000000);
    let s = t * t;
    let r = x / s;
    let r = (r - t) / (t + t + r);
    t + t * r
}
