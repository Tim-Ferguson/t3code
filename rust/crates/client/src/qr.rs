//! The pairing link's QR code. `QRCodeSvg` (`components/ui/qr-code.tsx`) draws the modules of `QrCode.encodeText`
//! (`packages/shared/src/qrCode.ts`, a port of Nayuki's generator, which `qrcodegen` is) as one path of one-module-high runs;
//! this builds the same path.
use qrcodegen::{QrCode, QrCodeEcc};

/// The error correction levels `QRCodeSvg` offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Low,
    Medium,
    Quartile,
    High,
}
impl Level {
    fn ecc(self) -> QrCodeEcc {
        match self {
            Self::Low => QrCodeEcc::Low,
            Self::Medium => QrCodeEcc::Medium,
            Self::Quartile => QrCodeEcc::Quartile,
            Self::High => QrCodeEcc::High,
        }
    }
}

/// A QR code ready to draw: `path` fills the dark modules inside a square view box of `view` units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drawing {
    pub view: i32,
    pub path: String,
}

/// The drawing of `text` with a quiet zone of `margin` modules, or none when the text is too long for any QR code.
/// The error correction is raised when it costs no size, as the original's encoder does.
pub fn draw(text: &str, level: Level, margin: i32) -> Option<Drawing> {
    let code = QrCode::encode_text(text, level.ecc()).ok()?;
    let size = code.size();
    let mut path = String::new();
    for y in 0..size {
        let mut run_start = -1;
        for x in 0..=size {
            let dark = x < size && code.get_module(x, y);
            if dark {
                if run_start == -1 {
                    run_start = x;
                }
                continue;
            }
            if run_start == -1 {
                continue;
            }
            path.push_str(&format!("M{} {}h{}v1H{}z", run_start + margin, y + margin, x - run_start, run_start + margin));
            run_start = -1;
        }
    }
    Some(Drawing { view: size + margin * 2, path })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn digest(path: &str) -> String {
        Sha256::digest(path.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The paths the original's own encoder draws (`QrCode.encodeText` and `buildQrPathData`, run under Node), by hash.
    #[test]
    fn the_modules_are_the_originals_for_the_same_text_level_and_margin() {
        let long = format!("{}\u{e9}\u{20ac}", "A".repeat(200));
        let cases: [(&str, Level, i32, i32, usize, &str); 4] = [
            ("http://192.168.1.20:3774/pair#token=abc123", Level::Medium, 1, 31, 2942, "535e3628614e3860bb468f1a926f25ae200a86ed9d06f3ebe854835e249fe159"),
            ("https://app.t3.codes/pair?host=https%3A%2F%2Fexample.ts.net&label=Kitchen#token=XYZ", Level::Medium, 2, 41, 4720, "048212aac830f1976871ce8f31c08e8c639b88935f96b71e6054ae51559adb29"),
            ("hello", Level::Low, 0, 21, 1374, "37306a810c545a3aa27ff1a28179ab776e419b5a7f4f58472fd1ec00d8b23260"),
            (&long, Level::Quartile, 1, 71, 16525, "cc9201dfaa582a807bbfbcdc8640194abf747026cfd02bde91ce0863b4324aba"),
        ];
        for (text, level, margin, view, length, hash) in cases {
            let drawing = draw(text, level, margin).unwrap();
            assert_eq!((drawing.view, drawing.path.len()), (view, length), "{text:.40}");
            assert_eq!(digest(&drawing.path), hash, "{text:.40}");
        }
        assert!(draw("hello", Level::Low, 0).unwrap().path.starts_with("M0 0h7v1H0zM12 0h1v1H12zM14 0h7v1H14z"));
    }

    #[test]
    fn text_too_long_for_any_code_has_no_drawing() {
        assert_eq!(draw(&"x".repeat(8000), Level::Medium, 1), None);
    }
}
