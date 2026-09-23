//! Byte-stream → SSE-block framing shared by the stream translators (SEC-16).
//!
//! Network reads don't align to block boundaries, so every translator used
//! to `push_str(from_utf8_lossy(chunk))` and search for `"\n\n"`. That had
//! three problems: a multi-byte character split across two reads turned
//! into two U+FFFD; a `\r\n\r\n`-framed stream never split at all; and an
//! upstream that never sent a separator grew the buffer without limit.

/// Largest amount of unframed text held while waiting for a separator. A
/// single SSE event this large is not a real LLM stream.
pub const MAX_SSE_BUFFER: usize = 16 * 1024 * 1024;

/// Decodes UTF-8 across chunk boundaries: an incomplete trailing sequence
/// is held until the next chunk instead of being replaced.
#[derive(Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);
        let mut out = String::with_capacity(input.len());
        let mut rest = &input[..];
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    // from_utf8 just validated this prefix, so the unwrap cannot fail.
                    out.push_str(std::str::from_utf8(&rest[..valid]).unwrap());
                    match e.error_len() {
                        Some(n) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            rest = &rest[valid + n..];
                        }
                        None => {
                            // Incomplete sequence at the end: wait for more.
                            self.pending = rest[valid..].to_vec();
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    /// End of stream: whatever is still pending is invalid.
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        String::from_utf8_lossy(&rest).into_owned()
    }
}

/// Splits a byte stream into SSE blocks, each returned with its trailing
/// `"\n\n"`. `\r\n` line endings are normalized to `\n`.
#[derive(Default)]
pub struct SseFramer {
    decoder: Utf8Decoder,
    buf: String,
    held_cr: bool,
}

impl SseFramer {
    /// Append a chunk. Returns `false` once the unframed buffer exceeds
    /// [`MAX_SSE_BUFFER`] - the caller should end the stream.
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        let mut text = self.decoder.decode(bytes);
        if std::mem::take(&mut self.held_cr) {
            text.insert(0, '\r');
        }
        // A `\r` at the very end may be the first half of a `\r\n`.
        if text.ends_with('\r') {
            text.pop();
            self.held_cr = true;
        }
        self.buf.push_str(&text.replace("\r\n", "\n"));
        self.buf.len() <= MAX_SSE_BUFFER
    }

    pub fn next_block(&mut self) -> Option<String> {
        let pos = self.buf.find("\n\n")?;
        Some(self.buf.drain(..pos + 2).collect())
    }

    /// Upstream ended: the unterminated remainder, if any.
    pub fn finish(&mut self) -> String {
        let mut rest = std::mem::take(&mut self.buf);
        rest.push_str(&self.decoder.finish());
        if std::mem::take(&mut self.held_cr) {
            rest.push('\r');
        }
        rest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multibyte_char_split_across_chunks_survives() {
        let bytes = "data: Việt\n\n".as_bytes();
        let split = bytes.iter().position(|&b| b >= 0x80).unwrap() + 1;
        let mut f = SseFramer::default();
        assert!(f.push(&bytes[..split]));
        assert!(f.next_block().is_none());
        assert!(f.push(&bytes[split..]));
        assert_eq!(f.next_block().unwrap(), "data: Việt\n\n");
    }

    #[test]
    fn crlf_framing_splits_even_across_chunks() {
        let mut f = SseFramer::default();
        assert!(f.push(b"data: a\r\n\r"));
        assert!(f.next_block().is_none());
        assert!(f.push(b"\ndata: b\r\n\r\n"));
        assert_eq!(f.next_block().unwrap(), "data: a\n\n");
        assert_eq!(f.next_block().unwrap(), "data: b\n\n");
        assert!(f.next_block().is_none());
    }

    #[test]
    fn unframed_growth_is_capped() {
        let mut f = SseFramer::default();
        let chunk = vec![b'x'; 1024 * 1024];
        let mut ok = true;
        for _ in 0..=(MAX_SSE_BUFFER / chunk.len()) {
            ok = f.push(&chunk);
        }
        assert!(!ok);
    }

    #[test]
    fn invalid_bytes_become_replacement_chars_and_finish_flushes() {
        let mut d = Utf8Decoder::default();
        assert_eq!(d.decode(b"a\xffb"), "a\u{fffd}b");
        assert_eq!(d.decode(b"\xe1\xbb"), "");
        assert_eq!(d.finish(), "\u{fffd}");
    }
}
