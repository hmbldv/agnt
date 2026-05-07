//! Vision tool — `look_at_screen`.
//!
//! Captures the current desktop, base64-encodes the PNG, and sends it as an
//! OpenAI-vision style chat completion to a multimodal endpoint (Qwen2.5-VL
//! served by vLLM on lnx-rig). Returns the model's text response capped at
//! 500 chars so it stays voice-friendly.
//!
//! ## Wire format
//!
//! The request body matches OpenAI's vision shape — content is a JSON array
//! with `image_url` and `text` parts. vLLM, llama-cpp-server, MoonDream,
//! and OpenAI itself all accept this. Some servers return `content` as a
//! plain string in the assistant message, others return an array of content
//! parts; the parser tolerates both.
//!
//! ## Threading model
//!
//! Like every other shell-tool here, this is sync `Tool::call` blocking on
//! the surrounding tokio runtime. The bridge always invokes tools from
//! `spawn_blocking`, so this is safe.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::shell::{block_on, run_blocking};

/// Default endpoint — qwen-vl on lnx-rig.
pub const DEFAULT_VISION_URL: &str = "http://lnx-rig:8002/v1/chat/completions";
/// Default model name advertised by the qwen-vl service. Currently the 2B
/// variant — the 3B model OOMs on the RTX 3080 Ti once whisper + kokoro
/// take their slice. If the 12 GB ceiling lifts, swap back to 3B.
pub const DEFAULT_VISION_MODEL: &str = "qwen2-vl-2b";
/// Default question if the agent doesn't supply one.
pub const DEFAULT_QUESTION: &str =
    "Describe in one sentence what's on the screen, focusing on the active window.";
/// Max chars in returned answer — voice replies must stay short.
pub const ANSWER_CHAR_CAP: usize = 500;
/// Vision-LLM call ceiling. Qwen-VL on a 3080 Ti takes ~3-6s for a 1MP image.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// Screenshot capture ceiling.
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct VisionConfig {
    pub url: String,
    pub model: String,
    /// Where transient screenshots live before they're uploaded. Same dir
    /// the `screenshot` tool uses; saved files are kept (not deleted) so
    /// the user can inspect what the model saw.
    pub cache_dir: PathBuf,
}

impl Default for VisionConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        Self {
            url: DEFAULT_VISION_URL.to_string(),
            model: DEFAULT_VISION_MODEL.to_string(),
            cache_dir: home.join(".cache/voicectl"),
        }
    }
}

/// Capture the screen, ask a vision LLM about it, return the answer.
pub struct LookAtScreen {
    cfg: VisionConfig,
}

impl LookAtScreen {
    pub fn new(cfg: VisionConfig) -> Self {
        Self { cfg }
    }

    fn screenshots_dir(&self) -> PathBuf {
        self.cfg.cache_dir.join("screenshots")
    }
}

impl agnt::Tool for LookAtScreen {
    fn name(&self) -> &str {
        "look_at_screen"
    }

    fn description(&self) -> &str {
        "Capture the current screen and ask a vision LLM about it. Use when \
         the user asks what's on their screen, what an error message says, \
         what's in a video they're watching, or where to click for X. The \
         vision model handles the image — you just need to ask it the right \
         focused question. Answer is capped to 500 characters (voice-friendly)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "What you want the vision model to tell you \
                                    about the screen. Be focused — 'what does \
                                    the error in the terminal say' beats \
                                    'describe everything'. Optional; defaults \
                                    to a short overview prompt."
                }
            }
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_QUESTION.to_string());

        // 1. Capture screenshot to disk.
        let dir = self.screenshots_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let path = dir.join(format!("look-{ts}.png"));
        capture_screen(&path)?;

        // 2. Read + base64-encode the PNG.
        let bytes =
            std::fs::read(&path).map_err(|e| format!("read screenshot {}: {e}", path.display()))?;
        let b64 = base64_encode(&bytes);
        let data_url = format!("data:image/png;base64,{b64}");

        // 3. Build request body.
        let body = build_request_body(&self.cfg.model, &data_url, &question);

        // 4. POST.
        let response = block_on(post_vision(&self.cfg.url, &body))?;

        // 5. Parse — tolerate both content-as-string and content-as-array.
        let answer = parse_answer(&response)?;
        Ok(truncate_chars(&answer, ANSWER_CHAR_CAP))
    }
}

/// Build the OpenAI-vision-shaped request body. Public for unit testing.
pub fn build_request_body(model: &str, image_data_url: &str, question: &str) -> Value {
    json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "image_url", "image_url": { "url": image_data_url } },
                { "type": "text", "text": question }
            ]
        }],
        "max_tokens": 200,
        "temperature": 0.2
    })
}

/// Parse the assistant message's content. OpenAI returns a string; some
/// vLLM builds + Anthropic-shaped responses return an array of content
/// parts with `{ "type": "text", "text": "…" }` items. Tolerate both.
pub fn parse_answer(body: &Value) -> Result<String, String> {
    let choices = body
        .get("choices")
        .and_then(|v| v.as_array())
        .ok_or("response has no 'choices' array")?;
    let first = choices.first().ok_or("response 'choices' array is empty")?;
    let content = first
        .get("message")
        .and_then(|m| m.get("content"))
        .ok_or("response choice has no message.content")?;

    if let Some(s) = content.as_str() {
        return Ok(s.trim().to_string());
    }
    if let Some(parts) = content.as_array() {
        let mut acc = String::new();
        for part in parts {
            // OpenAI / Anthropic style: { type: "text", text: "…" }
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    if !acc.is_empty() {
                        acc.push('\n');
                    }
                    acc.push_str(t);
                }
            } else if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                // Fallback: bare { text: "…" }.
                if !acc.is_empty() {
                    acc.push('\n');
                }
                acc.push_str(t);
            }
        }
        if acc.is_empty() {
            return Err("response content array contained no text parts".into());
        }
        return Ok(acc.trim().to_string());
    }
    Err("response message.content was neither string nor array".into())
}

async fn post_vision(url: &str, body: &Value) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| format!("build http client: {e}"))?;
    let resp = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read response body: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "vision endpoint returned {} — first 200 chars: {}",
            status,
            text.chars().take(200).collect::<String>()
        ));
    }
    serde_json::from_str(&text).map_err(|e| format!("decode response JSON: {e}"))
}

/// Run gnome-screenshot first, fall back to ImageMagick `import`. Mirrors
/// the existing `Screenshot` tool exactly so behaviour is consistent.
fn capture_screen(path: &Path) -> Result<(), String> {
    let path_s = path
        .to_str()
        .ok_or("screenshot path contained non-UTF-8 bytes")?;
    let gs = run_blocking("gnome-screenshot", ["-f", path_s], SCREENSHOT_TIMEOUT)?;
    if gs.status_ok && path.exists() {
        return Ok(());
    }
    let im = run_blocking("import", ["-window", "root", path_s], SCREENSHOT_TIMEOUT)?;
    if im.status_ok && path.exists() {
        return Ok(());
    }
    Err("no screenshot tool available (tried gnome-screenshot, import)".into())
}

/// Truncate `s` at `cap` chars (not bytes), appending an ellipsis when cut.
fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let mut out: String = s.chars().take(cap.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Minimal RFC 4648 base64 encoder. We don't take a `base64` dep just for
/// this one tool — the encoder is tiny and trivially auditable.
fn base64_encode(input: &[u8]) -> String {
    const ALPH: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let chunks = input.chunks_exact(3);
    let rem = chunks.remainder();
    for c in chunks {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
        out.push(ALPH[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPH[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPH[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPH[(n & 0x3f) as usize] as char);
    }
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(ALPH[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPH[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(ALPH[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPH[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPH[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_body_has_correct_openai_vision_shape() {
        let body = build_request_body("qwen2-vl-2b", "data:image/png;base64,AAAA", "what color");
        assert_eq!(body["model"], "qwen2-vl-2b");
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let content = messages[0]["content"].as_array().expect("content array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(content[0]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "what color");
        // max_tokens must be set or vLLM defaults to 16 — voice replies need
        // at least a few sentences of headroom.
        assert!(body["max_tokens"].as_u64().unwrap_or(0) >= 100);
    }

    #[test]
    fn parse_answer_handles_string_content() {
        let body = json!({
            "choices": [{
                "message": { "role": "assistant", "content": "the image is red." }
            }]
        });
        let s = parse_answer(&body).expect("parse");
        assert_eq!(s, "the image is red.");
    }

    #[test]
    fn parse_answer_handles_array_content() {
        // Some vLLM builds + Anthropic-shaped responses return content as
        // an array of typed parts.
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "the image is " },
                        { "type": "text", "text": "solid red." }
                    ]
                }
            }]
        });
        let s = parse_answer(&body).expect("parse");
        assert!(s.contains("solid red"));
        assert!(s.contains("the image is"));
    }

    #[test]
    fn parse_answer_errors_on_empty_choices() {
        let body = json!({ "choices": [] });
        let err = parse_answer(&body).unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn parse_answer_errors_on_missing_content() {
        let body = json!({ "choices": [{ "message": {} }] });
        let err = parse_answer(&body).unwrap_err();
        assert!(err.contains("content"));
    }

    #[test]
    fn truncate_chars_caps_long_input() {
        let long: String = "a".repeat(1000);
        let out = truncate_chars(&long, 500);
        assert_eq!(out.chars().count(), 500);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_chars_passes_through_short_input() {
        let s = "short answer";
        assert_eq!(truncate_chars(s, 500), "short answer");
    }

    #[test]
    fn base64_round_trip_examples() {
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encodes_png_signature_correctly() {
        // First 8 bytes of every PNG file.
        let png_sig = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let encoded = base64_encode(&png_sig);
        // Round-trip via a known-correct decoder (tested against another
        // implementation): "iVBORw0KGgo=" is the canonical PNG-magic b64.
        assert_eq!(encoded, "iVBORw0KGgo=");
    }

    #[test]
    fn vision_config_default_targets_lnx_rig() {
        let cfg = VisionConfig::default();
        assert!(cfg.url.contains("lnx-rig"), "url={}", cfg.url);
        assert!(cfg.url.contains(":8002"), "url={}", cfg.url);
        assert_eq!(cfg.model, "qwen2-vl-2b");
    }

    /// Build a `size x size` PNG with a red square centred on a black
    /// background. Qwen-VL hallucinates wildly on uniform-colour images
    /// (the ViT normalises away the only signal in the input), so a
    /// figure-on-ground pattern is what we send for the live test.
    fn make_solid_red_png(size: u32) -> Vec<u8> {
        // PNG = 8-byte signature + IHDR chunk + IDAT chunk + IEND chunk.
        fn chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(12 + data.len());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(tag);
            out.extend_from_slice(data);
            // CRC32 over tag+data using flate2's adler? Use the std crc32fast
            // alternative? We don't have that dep — write a small CRC32.
            let mut table = [0u32; 256];
            for (n, slot) in table.iter_mut().enumerate() {
                let mut c = n as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        0xedb8_8320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                }
                *slot = c;
            }
            let mut crc = 0xffff_ffff_u32;
            for b in tag.iter().chain(data.iter()) {
                crc = table[((crc ^ *b as u32) & 0xff) as usize] ^ (crc >> 8);
            }
            crc ^= 0xffff_ffff;
            out.extend_from_slice(&crc.to_be_bytes());
            out
        }

        let mut ihdr_data = Vec::with_capacity(13);
        ihdr_data.extend_from_slice(&size.to_be_bytes());
        ihdr_data.extend_from_slice(&size.to_be_bytes());
        ihdr_data.extend_from_slice(&[8, 2, 0, 0, 0]); // bit-depth=8, color=2 (RGB), rest default

        // Raw scanlines: filter byte 0 + size*3 bytes. Red square covers
        // the central 50% of the canvas; everything else is black. This
        // gives the ViT something to latch onto.
        let q = size / 4;
        let inner_lo = q;
        let inner_hi = size - q;
        let mut raw = Vec::with_capacity(size as usize * (1 + size as usize * 3));
        for y in 0..size {
            raw.push(0u8);
            for x in 0..size {
                let in_square = x >= inner_lo && x < inner_hi && y >= inner_lo && y < inner_hi;
                if in_square {
                    raw.extend_from_slice(&[0xff, 0x00, 0x00]);
                } else {
                    raw.extend_from_slice(&[0x00, 0x00, 0x00]);
                }
            }
        }
        // Minimal zlib wrapper: 0x78 0x01 (no compression), then DEFLATE
        // stored blocks. We don't take a flate2 dep just for this test, so
        // hand-roll an uncompressed deflate stream.
        let compressed = deflate_uncompressed(&raw);

        let mut png = Vec::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        png.extend_from_slice(&chunk(b"IHDR", &ihdr_data));
        png.extend_from_slice(&chunk(b"IDAT", &compressed));
        png.extend_from_slice(&chunk(b"IEND", b""));
        png
    }

    /// Hand-rolled "stored" zlib stream: header + uncompressed DEFLATE
    /// blocks + Adler-32 trailer. Avoids a flate2 dep — PNG decoders accept
    /// any valid zlib stream regardless of compression strategy.
    fn deflate_uncompressed(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() + 16);
        out.extend_from_slice(&[0x78, 0x01]); // zlib header (no compression)
                                              // Stored blocks of <=65535 bytes each.
        let mut i = 0;
        while i < input.len() {
            let n = (input.len() - i).min(0xffff);
            let last = (i + n) == input.len();
            out.push(if last { 1 } else { 0 }); // BFINAL + BTYPE=00
            out.extend_from_slice(&(n as u16).to_le_bytes());
            out.extend_from_slice(&(!(n as u16)).to_le_bytes());
            out.extend_from_slice(&input[i..i + n]);
            i += n;
        }
        // Adler-32 trailer.
        let (mut a, mut b) = (1u32, 0u32);
        for byte in input {
            a = (a + *byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        let adler = (b << 16) | a;
        out.extend_from_slice(&adler.to_be_bytes());
        out
    }

    /// Live integration test — requires qwen-vl up at the configured URL.
    /// Sends a synthetic 256x256 solid-red PNG and asserts "red" is in the
    /// response. Run with: `cargo test -p agnt-bridge-tools live_red -- --ignored`.
    ///
    /// Uses 256x256 because Qwen2-VL-2B's image encoder ignores degenerate
    /// images smaller than ~32x32; with very low res it tends to hallucinate
    /// a scene rather than report the dominant color.
    #[tokio::test]
    #[ignore = "requires live qwen-vl at http://lnx-rig:8002"]
    async fn live_red_image_says_red() {
        let png = make_solid_red_png(256);
        let b64 = base64_encode(&png);
        let data_url = format!("data:image/png;base64,{b64}");
        let body = build_request_body(
            DEFAULT_VISION_MODEL,
            &data_url,
            "Describe this image in one sentence.",
        );
        let resp = post_vision(DEFAULT_VISION_URL, &body)
            .await
            .expect("post vision");
        let answer = parse_answer(&resp).expect("parse");
        let lower = answer.to_lowercase();
        assert!(
            lower.contains("red"),
            "expected 'red' in response, got: {answer}"
        );
    }

    #[test]
    fn synthetic_red_png_has_correct_signature() {
        // The hand-rolled PNG generator must produce a valid PNG signature
        // and IHDR width+height; without this guarantee the live test can't
        // distinguish "model wrong" from "image malformed".
        let png = make_solid_red_png(64);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        // IHDR starts at byte 8, length is 4 bytes, tag is 4 bytes,
        // then width (4) + height (4).
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes([png[16], png[17], png[18], png[19]]), 64);
        assert_eq!(u32::from_be_bytes([png[20], png[21], png[22], png[23]]), 64);
    }
}
