//! Incremental node-pty/StringDecoder-compatible UTF-8 decoding. A raw PTY
//! read boundary is not a text boundary; incomplete bytes await the next read.
#[derive(Default)]
pub struct TerminalUtf8Decoder {
    pending: Vec<u8>,
}
impl TerminalUtf8Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        // StringDecoder determines incomplete suffixes from byte classes,
        // including invalid C0/C1 and F5..F7 lead bytes. Decode validity and
        // replacement are evaluated when the whole nominal sequence arrives.
        let mut cutoff = self.pending.len();
        for distance in 1..=self.pending.len().min(3) {
            let index = self.pending.len() - distance;
            let byte = self.pending[index];
            let length = match byte {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 0,
            };
            if length > 0 {
                if distance < length {
                    cutoff = index;
                }
                break;
            }
            if !(0x80..=0xbf).contains(&byte) {
                break;
            }
        }
        let text = String::from_utf8_lossy(&self.pending[..cutoff]).into_owned();
        self.pending.drain(..cutoff);
        text
    }
    pub fn finish(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_raw_byte_split_and_invalid_sequence_matches_node_string_decoder() {
        let fixtures: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/terminal-utf8.json")).unwrap();
        for (index, fixture) in fixtures.iter().enumerate() {
            let mut decoder = TerminalUtf8Decoder::default();
            let mut outputs = Vec::new();
            for chunk in fixture["chunks"].as_array().unwrap() {
                let chunk = chunk
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|b| b.as_u64().unwrap() as u8)
                    .collect::<Vec<_>>();
                outputs.push(decoder.feed(&chunk));
            }
            outputs.push(decoder.finish());
            assert_eq!(
                serde_json::json!(outputs),
                fixture["outputs"],
                "UTF-8 fixture{index}"
            );
            assert!(decoder.pending.is_empty());
        }
    }
}
