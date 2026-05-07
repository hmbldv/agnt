//! Strip OpenAI Harmony-style channel markers from an LLM token stream.
//!
//! Some chat-trained models (gemma4-26b on vLLM, gpt-oss variants) emit
//! channel-separator markers as plain text on the assistant content stream
//! instead of as proper tokenizer special tokens, e.g.
//!
//! ```text
//! <|channel>thought
//! <channel|>The user is doop.
//! ```
//!
//! Without filtering, those markers get pumped verbatim into the TTS
//! pipeline and the user hears literal "channel … thought … channel".
//!
//! ## Format recognised
//!
//! - `<|channel>` opens a non-final channel. Everything until the next
//!   `<channel|>` is dropped (this is the model's chain-of-thought).
//! - `<channel|>` closes the channel; content after it is forwarded.
//!
//! The canonical Harmony delimiter `<|channel|>` is also accepted as both
//! open and close so a model that emits the symmetric form is also handled.
//!
//! ## Streaming
//!
//! [`HarmonyStripper`] is stateful: it correctly handles markers split
//! across multiple deltas. To do that it holds back the last few bytes of
//! each delta in case they are the prefix of a marker, and flushes them
//! on the next delta or via [`HarmonyStripper::flush`] at end-of-stream.

/// Markers that open a non-final (e.g. thought / analysis) channel. The
/// stripper drops everything from the start of any of these up to the next
/// close marker.
const OPEN_MARKERS: &[&str] = &["<|channel>", "<|channel|>"];

/// Markers that close a non-final channel. The stripper resumes forwarding
/// content after any of these.
const CLOSE_MARKERS: &[&str] = &["<channel|>", "<|channel|>"];

/// Maximum byte length of any marker. We hold back this many bytes on every
/// non-thought emit in case the tail is the prefix of an incomplete marker.
const MAX_MARKER_LEN: usize = 10;

#[derive(Debug, Default)]
pub struct HarmonyStripper {
    buf: String,
    in_thought: bool,
}

impl HarmonyStripper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a delta. Returns the cleaned portion that is safe to forward
    /// downstream now. Bytes that might be the prefix of a marker are kept
    /// in the internal buffer until the next [`Self::process`] call or
    /// until [`Self::flush`].
    pub fn process(&mut self, delta: &str) -> String {
        if delta.is_empty() {
            return String::new();
        }
        self.buf.push_str(delta);
        let mut out = String::new();
        loop {
            if self.in_thought {
                if let Some((idx, len)) = find_first_marker(&self.buf, CLOSE_MARKERS) {
                    self.buf.replace_range(..idx + len, "");
                    self.in_thought = false;
                    continue;
                }
                let keep_from = safe_split_at(&self.buf, MAX_MARKER_LEN);
                self.buf.replace_range(..keep_from, "");
                return out;
            }
            if let Some((idx, len)) = find_first_marker(&self.buf, OPEN_MARKERS) {
                out.push_str(&self.buf[..idx]);
                self.buf.replace_range(..idx + len, "");
                self.in_thought = true;
                continue;
            }
            let safe_end = safe_split_at(&self.buf, MAX_MARKER_LEN);
            out.push_str(&self.buf[..safe_end]);
            self.buf.replace_range(..safe_end, "");
            return out;
        }
    }

    /// Emit any remaining non-thought content. Buffered thought content is
    /// discarded. Call this at end-of-stream to release the few held-back
    /// bytes that the tail-clip in [`Self::process`] preserves.
    pub fn flush(&mut self) -> String {
        if self.in_thought {
            self.buf.clear();
            return String::new();
        }
        std::mem::take(&mut self.buf)
    }

    /// One-shot helper for non-streaming callers (e.g. cleaning the final
    /// `AgentReply.text` before publishing it).
    pub fn strip_full(text: &str) -> String {
        let mut s = Self::new();
        let mut out = s.process(text);
        out.push_str(&s.flush());
        out
    }
}

/// Find the earliest occurrence of any marker in `buf`. Returns
/// `(byte_offset, marker_byte_len)` of the first match, or `None`.
fn find_first_marker(buf: &str, markers: &[&str]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for m in markers {
        if let Some(i) = buf.find(m) {
            best = match best {
                Some((bi, _)) if bi <= i => best,
                _ => Some((i, m.len())),
            };
        }
    }
    best
}

/// Return the largest byte index `<= buf.len() - tail` that lies on a UTF-8
/// char boundary. Used so we never split a multi-byte char when holding back
/// a tail of `tail` bytes for marker-prefix matching.
fn safe_split_at(buf: &str, tail: usize) -> usize {
    if buf.len() <= tail {
        return 0;
    }
    let target = buf.len() - tail;
    if buf.is_char_boundary(target) {
        return target;
    }
    // Walk backwards to the nearest char boundary.
    (0..target).rev().find(|&i| buf.is_char_boundary(i)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_no_markers() {
        let mut s = HarmonyStripper::new();
        let mut got = s.process("Hello world.");
        got.push_str(&s.flush());
        assert_eq!(got, "Hello world.");
    }

    #[test]
    fn strips_full_marker_block_in_one_delta() {
        let mut s = HarmonyStripper::new();
        let mut got = s.process("<|channel>thought\n<channel|>Hello.");
        got.push_str(&s.flush());
        assert_eq!(got, "Hello.");
    }

    #[test]
    fn drops_block_with_no_after_content() {
        let mut s = HarmonyStripper::new();
        let mut got = s.process("<|channel>thought\n<channel|>");
        got.push_str(&s.flush());
        assert_eq!(got, "");
    }

    #[test]
    fn marker_split_across_deltas_open() {
        let mut s = HarmonyStripper::new();
        // Open marker split: "<|cha" then "nnel>thought<channel|>Hi."
        let a = s.process("<|cha");
        let b = s.process("nnel>thought<channel|>Hi.");
        let c = s.flush();
        assert_eq!(a + &b + &c, "Hi.");
    }

    #[test]
    fn marker_split_across_deltas_close() {
        let mut s = HarmonyStripper::new();
        let a = s.process("<|channel>thought<chan");
        let b = s.process("nel|>Hi.");
        let c = s.flush();
        assert_eq!(a + &b + &c, "Hi.");
    }

    #[test]
    fn multiple_thought_blocks_in_one_stream() {
        let mut s = HarmonyStripper::new();
        let mut got =
            s.process("<|channel>a<channel|>middle.<|channel>b<channel|>end.");
        got.push_str(&s.flush());
        assert_eq!(got, "middle.end.");
    }

    #[test]
    fn content_before_first_marker_is_preserved() {
        let mut s = HarmonyStripper::new();
        let mut got =
            s.process("Pre.<|channel>thought\n<channel|>Post.");
        got.push_str(&s.flush());
        assert_eq!(got, "Pre.Post.");
    }

    #[test]
    fn streaming_one_byte_at_a_time_recovers_full_message() {
        let input = "<|channel>thought\n<channel|>The user is doop.";
        let mut s = HarmonyStripper::new();
        let mut out = String::new();
        for ch in input.chars() {
            let mut tmp = [0u8; 4];
            out.push_str(&s.process(ch.encode_utf8(&mut tmp)));
        }
        out.push_str(&s.flush());
        assert_eq!(out, "The user is doop.");
    }

    #[test]
    fn flush_discards_unterminated_thought() {
        let mut s = HarmonyStripper::new();
        let mut got = s.process("<|channel>thought without close");
        got.push_str(&s.flush());
        assert_eq!(got, "");
    }

    #[test]
    fn symmetric_canonical_marker_pair() {
        let mut s = HarmonyStripper::new();
        let mut got = s.process("<|channel|>thought<|channel|>Final.");
        got.push_str(&s.flush());
        assert_eq!(got, "Final.");
    }

    #[test]
    fn strip_full_helper() {
        let cleaned = HarmonyStripper::strip_full(
            "<|channel>thought\n<channel|>The user is doop.",
        );
        assert_eq!(cleaned, "The user is doop.");
    }

    #[test]
    fn empty_delta_is_noop() {
        let mut s = HarmonyStripper::new();
        assert_eq!(s.process(""), "");
        assert_eq!(s.flush(), "");
    }

    #[test]
    fn utf8_in_content_does_not_split_codepoints() {
        let mut s = HarmonyStripper::new();
        // 'é' is two bytes; if MAX_MARKER_LEN tail-clip lands inside it the
        // safe_split_at helper must back up to the previous char boundary.
        let mut got = s.process("café résumé naïve");
        got.push_str(&s.flush());
        assert_eq!(got, "café résumé naïve");
    }
}
