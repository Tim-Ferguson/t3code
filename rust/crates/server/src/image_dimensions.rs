//! Pixel size from the header bytes of a PNG, JPEG, GIF, or WebP, so a client can reserve
//! the exact box before the bytes arrive (packages/shared/src/imageDimensions.ts). Anything
//! else, a truncated header, or a malformed file yields None.

/// Enough for every supported header: a JPEG's frame header can sit behind several
/// 64 KiB metadata segments (EXIF, an ICC profile, XMP).
pub const IMAGE_DIMENSIONS_HEADER_BYTES: usize = 256 * 1024;

pub fn read_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    read_png(bytes)
        .or_else(|| read_gif(bytes))
        .or_else(|| read_webp(bytes))
        .or_else(|| read_jpeg(bytes))
        .filter(|(width, height)| *width > 0 && *height > 0)
}

/// Reads the header of a regular file; anything else (a FIFO, a directory) yields None.
pub fn read_image_dimensions_from_file(path: &std::path::Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut header = Vec::new();
    file.take(IMAGE_DIMENSIONS_HEADER_BYTES as u64)
        .read_to_end(&mut header)
        .ok()?;
    read_image_dimensions(&header)
}

fn u16_at(bytes: &[u8], offset: usize, little: bool) -> Option<u16> {
    let pair: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
    Some(if little {
        u16::from_le_bytes(pair)
    } else {
        u16::from_be_bytes(pair)
    })
}
fn u32_at(bytes: &[u8], offset: usize, little: bool) -> Option<u32> {
    let quad: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(quad)
    } else {
        u32::from_be_bytes(quad)
    })
}

fn read_png(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24
        || bytes[..4] != [0x89, 0x50, 0x4e, 0x47]
        || bytes[12..16] != [0x49, 0x48, 0x44, 0x52]
    {
        return None;
    }
    Some((u32_at(bytes, 16, false)?, u32_at(bytes, 20, false)?))
}

fn read_gif(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 10 || bytes[..4] != [0x47, 0x49, 0x46, 0x38] {
        return None;
    }
    Some((
        u16_at(bytes, 6, true)? as u32,
        u16_at(bytes, 8, true)? as u32,
    ))
}

fn read_webp(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 16 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    match &bytes[12..16] {
        b"VP8 " => {
            if bytes.len() < 30 {
                return None;
            }
            // Lossy: 14-bit dimensions after the 3-byte frame tag and 3-byte start code.
            Some((
                (u16_at(bytes, 26, true)? & 0x3fff) as u32,
                (u16_at(bytes, 28, true)? & 0x3fff) as u32,
            ))
        }
        b"VP8L" => {
            if bytes.len() < 25 {
                return None;
            }
            // Lossless: width-1 in bits 0-13 and height-1 in bits 14-27 after the signature byte.
            let packed = u32_at(bytes, 21, true)?;
            Some(((packed & 0x3fff) + 1, ((packed >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => {
            if bytes.len() < 30 {
                return None;
            }
            // Extended: 24-bit canvas dimensions minus one.
            let wide = |at: usize| {
                bytes[at] as u32 | (bytes[at + 1] as u32) << 8 | (bytes[at + 2] as u32) << 16
            };
            Some((wide(24) + 1, wide(27) + 1))
        }
        _ => None,
    }
}

fn read_jpeg(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut offset = 2;
    let mut rotated = false;
    while offset + 9 <= bytes.len() {
        if bytes[offset] != 0xff {
            return None;
        }
        let marker = bytes[offset + 1];
        // Padding bytes between segments.
        if marker == 0xff {
            offset += 1;
            continue;
        }
        // Start-of-frame markers carry the dimensions; skip the arithmetic-coding and
        // Huffman-table markers that share the range.
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let height = u16_at(bytes, offset + 5, false)? as u32;
            let width = u16_at(bytes, offset + 7, false)? as u32;
            return Some(if rotated {
                (height, width)
            } else {
                (width, height)
            });
        }
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        // TEM and the restart markers stand alone, with no length field.
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            offset += 2;
            continue;
        }
        let length = u16_at(bytes, offset + 2, false)? as usize;
        // Viewers apply the EXIF orientation before display, so a phone photo stored on its
        // side takes the swapped size on screen.
        if marker == 0xe1 && !rotated {
            rotated = exif_orientation_swaps_axes(bytes, offset + 4, offset + 2 + length);
        }
        offset += 2 + length;
    }
    None
}

/// Whether EXIF orientation 5-8 (a 90 degree rotation) applies. `start` is the APP1 payload.
fn exif_orientation_swaps_axes(bytes: &[u8], start: usize, end: usize) -> bool {
    let end = end.min(bytes.len());
    if end < start + 14 || &bytes[start..start + 4] != b"Exif" {
        return false;
    }
    let tiff = start + 6;
    let little = bytes[tiff] == 0x49 && bytes[tiff + 1] == 0x49;
    if !little && !(bytes[tiff] == 0x4d && bytes[tiff + 1] == 0x4d) {
        return false;
    }
    let Some(ifd) = u32_at(bytes, tiff + 4, little).map(|offset| tiff + offset as usize) else {
        return false;
    };
    if ifd + 2 > end {
        return false;
    }
    let Some(entries) = u16_at(bytes, ifd, little) else {
        return false;
    };
    for index in 0..entries as usize {
        let entry = ifd + 2 + index * 12;
        if entry + 12 > end {
            return false;
        }
        if u16_at(bytes, entry, little) == Some(0x0112) {
            return u16_at(bytes, entry + 8, little)
                .is_some_and(|orientation| (5..=8).contains(&orientation));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Samples and results come from the original reader (generate_attachment_access_fixtures.mjs).
    #[test]
    fn original_reader_and_port_agree_on_generated_headers() {
        let rows = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/image-dimensions.jsonl"
        ))
        .unwrap();
        let mut count = 0;
        for line in rows.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let hex = row["hex"].as_str().unwrap();
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
                .collect();
            let expected = row["dimensions"].as_object().map(|size| {
                (
                    size["width"].as_u64().unwrap() as u32,
                    size["height"].as_u64().unwrap() as u32,
                )
            });
            assert_eq!(read_image_dimensions(&bytes), expected, "{}", row["label"]);
            count += 1;
        }
        assert!(count > 25, "fixture lost coverage");
    }
}
