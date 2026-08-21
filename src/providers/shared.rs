//! Shared utilities for providers: HTTP auth helpers, streaming UTF-8 decode.

// ── Auth ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AuthStyle {
    Bearer,
    XApiKey,
}

pub fn build_auth(auth: &AuthStyle, credential: &str) -> String {
    match auth {
        AuthStyle::Bearer => format!("Bearer {}", credential),
        AuthStyle::XApiKey => credential.to_string(),
    }
}

// ── Utf8StreamDecoder ────────────────────────────────────────────────────────

/// Diagnostic for a genuinely invalid (not just incomplete) byte sequence
/// encountered mid-stream.
#[derive(Debug, Clone, Copy)]
pub struct InvalidUtf8 {
    pub valid_up_to: usize,
    pub bad_len: usize,
}

/// Incrementally decodes a byte stream to UTF-8 text across network-chunk
/// boundaries, correctly buffering an incomplete trailing multi-byte
/// sequence instead of discarding it.
///
/// `std::str::from_utf8` on a buffer that ends mid-character reports
/// `Utf8Error::error_len() == None` — the trailing bytes aren't invalid,
/// they just aren't complete yet. Four provider SSE readers used to
/// `.clear()` the whole byte buffer on *any* decode error, discarding
/// those valid-but-incomplete trailing bytes. When a network chunk
/// boundary split a multi-byte character — routine with CJK text, e.g. a
/// tool-call argument string containing Chinese — the next chunk's bytes
/// got appended to a now-empty buffer instead of completing the split
/// sequence, corrupting the reconstructed text and, for SSE, the JSON on
/// that `data:` line. This was the root cause behind issue #91 (SSE chunk
/// JSON parse failures that silently dropped tool calls).
#[derive(Default)]
pub struct Utf8StreamDecoder {
    buf: Vec<u8>,
}

impl Utf8StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed newly-read bytes in. Returns the text decoded so far (empty if
    /// `bytes` only extended an incomplete trailing sequence that's still
    /// incomplete) and any genuinely invalid byte sequences skipped along
    /// the way (rare — real encoding errors, not just boundary splits) for
    /// the caller to log. Loops internally so multiple invalid runs in one
    /// push are all handled before returning, rather than trickling out
    /// one per call.
    pub fn push(&mut self, bytes: &[u8]) -> (String, Vec<InvalidUtf8>) {
        self.buf.extend_from_slice(bytes);
        let mut out = String::new();
        let mut diagnostics = Vec::new();
        loop {
            match std::str::from_utf8(&self.buf) {
                Ok(s) => {
                    out.push_str(s);
                    self.buf.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    match e.error_len() {
                        None => {
                            // Incomplete trailing sequence: too short to
                            // even guess how many bytes it needs — wait
                            // for more.
                            if valid == 0 && self.buf.len() < 4 {
                                break;
                            }
                            out.push_str(
                                std::str::from_utf8(&self.buf[..valid])
                                    .expect("prefix is valid by definition of valid_up_to()"),
                            );
                            self.buf.drain(..valid);
                            break;
                        }
                        Some(bad_len) => {
                            out.push_str(
                                std::str::from_utf8(&self.buf[..valid])
                                    .expect("prefix is valid by definition of valid_up_to()"),
                            );
                            diagnostics.push(InvalidUtf8 {
                                valid_up_to: valid,
                                bad_len,
                            });
                            self.buf.drain(..valid + bad_len);
                            // Loop: there may be more valid text (or more
                            // bad bytes) left in the buffer.
                        }
                    }
                }
            }
        }
        (out, diagnostics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_decodes_immediately() {
        let mut d = Utf8StreamDecoder::new();
        let (text, invalid) = d.push(b"hello world");
        assert_eq!(text, "hello world");
        assert!(invalid.is_empty());
    }

    #[test]
    fn multibyte_char_split_across_two_pushes_decodes_correctly() {
        // "中" (U+4E2D) is E4 B8 AD in UTF-8 — split after the first byte,
        // exactly the shape of a chunk boundary landing mid-character.
        let full = "工具参数：中文内容".as_bytes().to_vec();
        let split_at = 1; // splits the very first CJK char's first byte off
        let mut d = Utf8StreamDecoder::new();
        let (first, invalid1) = d.push(&full[..split_at]);
        assert!(invalid1.is_empty());
        // Nothing decodable yet from a single lone continuation-expecting byte.
        assert!(first.is_empty(), "got: {first:?}");
        let (second, invalid2) = d.push(&full[split_at..]);
        assert!(invalid2.is_empty());
        assert_eq!(first + &second, "工具参数：中文内容");
    }

    #[test]
    fn multibyte_char_split_mid_sequence_across_two_pushes() {
        // Split after 2 of "中"'s 3 bytes — the boundary this bug actually
        // hit in production (a 3-byte CJK char split 2+1 across chunks).
        let full = "中".as_bytes().to_vec();
        assert_eq!(full.len(), 3);
        let mut d = Utf8StreamDecoder::new();
        let (first, invalid1) = d.push(&full[..2]);
        assert!(invalid1.is_empty());
        assert!(first.is_empty());
        let (second, invalid2) = d.push(&full[2..]);
        assert!(invalid2.is_empty());
        assert_eq!(second, "中");
    }

    #[test]
    fn text_before_and_after_a_split_char_is_preserved() {
        let full = "before 中 after".as_bytes().to_vec();
        let split_at = full.len() - 4; // lands inside "中"'s 3 bytes
        let mut d = Utf8StreamDecoder::new();
        let (first, _) = d.push(&full[..split_at]);
        let (second, _) = d.push(&full[split_at..]);
        assert_eq!(first + &second, "before 中 after");
    }

    #[test]
    fn genuinely_invalid_byte_sequence_is_skipped_with_diagnostic() {
        let mut bytes = b"before ".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE]); // not valid UTF-8 anywhere
        bytes.extend_from_slice(b" after");
        let mut d = Utf8StreamDecoder::new();
        let (text, invalid) = d.push(&bytes);
        assert!(text.starts_with("before "), "got: {text:?}");
        assert!(text.ends_with(" after"), "got: {text:?}");
        assert!(
            !invalid.is_empty(),
            "expected at least one invalid-byte diagnostic"
        );
        // The internal loop must fully resync within this same push — no
        // invalid bytes left dangling in the buffer for the next call.
        let (more, invalid2) = d.push(b" more");
        assert_eq!(more, " more");
        assert!(invalid2.is_empty());
    }
}
