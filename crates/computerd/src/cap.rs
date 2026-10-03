//! Keeps long text readable by cutting out the middle.

use std::collections::VecDeque;

/// Bytes kept from the start of a stream.
pub const HEAD_BYTES: usize = 15_000;

/// Bytes kept from the end of a stream.
pub const TAIL_BYTES: usize = 15_000;

/// Reads text of any length, in chunks, while holding at most [`HEAD_BYTES`] + [`TAIL_BYTES`] of it.
#[derive(Debug, Default)]
pub struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
}

impl Capture {
    pub fn push(&mut self, mut chunk: &[u8]) {
        self.total += chunk.len() as u64;
        if self.head.len() < HEAD_BYTES {
            let (keep, rest) = chunk.split_at(chunk.len().min(HEAD_BYTES - self.head.len()));
            self.head.extend_from_slice(keep);
            chunk = rest;
        }
        let chunk = &chunk[chunk.len().saturating_sub(TAIL_BYTES)..];
        self.tail.extend(chunk);
        let excess = self.tail.len().saturating_sub(TAIL_BYTES);
        self.tail.drain(..excess);
    }

    /// The text read so far. Invalid UTF-8 becomes U+FFFD. When bytes were dropped, a marker
    /// says how many. A character cut by the marker is dropped whole.
    #[must_use]
    pub fn into_text(self) -> String {
        let kept = (self.head.len() + self.tail.len()) as u64;
        let tail = self.tail.into_iter().collect::<Vec<_>>();
        if self.total == kept {
            let mut all = self.head;
            all.extend(tail);
            return String::from_utf8_lossy(&all).into_owned();
        }
        let head = &self.head[..complete_prefix_len(&self.head)];
        let tail = &tail[continuation_prefix_len(&tail)..];
        let omitted = self.total - head.len() as u64 - tail.len() as u64;
        format!(
            "{}\n[... {omitted} bytes omitted ...]\n{}",
            String::from_utf8_lossy(head),
            String::from_utf8_lossy(tail)
        )
    }
}

/// Length of `bytes` without a trailing, cut-off UTF-8 sequence.
fn complete_prefix_len(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        _ => bytes.len(),
    }
}

/// Number of leading UTF-8 continuation bytes, which belong to a character cut off before them.
fn continuation_prefix_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take(3)
        .take_while(|byte| (0x80..0xC0).contains(*byte))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap_text(text: &str) -> String {
        let mut capture = Capture::default();
        capture.push(text.as_bytes());
        capture.into_text()
    }

    #[test]
    fn short_text_is_returned_whole() {
        let text = "héllo\nwörld\n".repeat(100);
        assert_eq!(cap_text(&text), text);
    }

    #[test]
    fn long_output_keeps_both_ends_and_counts_what_was_cut() {
        let mut capture = Capture::default();
        capture.push(b"START");
        for _ in 0..1000 {
            capture.push(&[b'x'; 1000]);
        }
        capture.push(b"END");
        let total = 5 + 1_000_000 + 3;
        let text = capture.into_text();
        let omitted = total - HEAD_BYTES - TAIL_BYTES;
        let marker = format!("\n[... {omitted} bytes omitted ...]\n");
        assert!(text.starts_with("START"));
        assert!(text.ends_with("END"));
        assert_eq!(text.len(), HEAD_BYTES + TAIL_BYTES + marker.len());
        assert!(text.contains(&marker));
    }

    #[test]
    fn chunk_sizes_do_not_change_the_result() {
        let data: Vec<u8> = (0..100_000u32)
            .map(|n| b'a' + u8::try_from(n % 26).unwrap())
            .collect();
        let whole = {
            let mut capture = Capture::default();
            capture.push(&data);
            capture.into_text()
        };
        for size in [1, 7, 4096, 20_000, 99_999] {
            let mut capture = Capture::default();
            for chunk in data.chunks(size) {
                capture.push(chunk);
            }
            assert_eq!(capture.into_text(), whole, "chunk size {size}");
        }
    }

    #[test]
    fn cuts_never_split_a_character_or_invent_replacement_marks() {
        let text = "é".repeat(30_000);
        let capped = cap_text(&text);
        assert!(!capped.contains('\u{FFFD}'));
        assert!(capped.contains("bytes omitted"));
    }

    #[test]
    fn invalid_bytes_become_replacement_characters() {
        let mut capture = Capture::default();
        capture.push(b"a\xFFb");
        assert_eq!(capture.into_text(), "a\u{FFFD}b");
    }
}
