//! Vision tool — `look_at_screen`.
//!
//! Captures the current desktop, base64-encodes the PNG, and dispatches a
//! [`vzn_core::wire::VznRequest`] over NATS to `vzn.request`. vznd handles
//! model selection and returns a [`vzn_core::wire::VznReply`] on the per-
//! request reply subject. Returns the reply text capped at 500 chars so it
//! stays voice-friendly.
//!
//! ## NATS dispatch flow
//!
//! 1. Subscribe to `vzn.reply.<request_id>` (before publishing, to avoid a
//!    race).
//! 2. Publish `VznRequest` to `vzn.request`.
//! 3. Wait up to 30 s for a `VznReply` message on the reply subject.
//! 4. Return `reply.text` or an error.
//!
//! ## Threading model
//!
//! Like every other shell-tool here, this is sync `Tool::call` blocking on
//! the surrounding tokio runtime. The bridge always invokes tools from
//! `spawn_blocking`, so this is safe.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tracing::debug;
use uuid::Uuid;

use vzn_core::wire::{ImageRef, VznReply, VznRequest};

use crate::shell::{block_on, run_blocking};

/// Default question if the agent doesn't supply one.
pub const DEFAULT_QUESTION: &str =
    "Describe in one sentence what's on the screen, focusing on the active window.";
/// Max chars in returned answer — voice replies must stay short.
pub const ANSWER_CHAR_CAP: usize = 500;
/// How long to wait for vznd to reply.
const VZN_TIMEOUT: Duration = Duration::from_secs(30);
/// Screenshot capture ceiling.
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct VisionConfig {
    /// Where transient screenshots live before they're sent. Same dir the
    /// `screenshot` tool uses; files are kept (not deleted) so the user can
    /// inspect what vznd saw.
    pub cache_dir: PathBuf,
}

impl Default for VisionConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        Self {
            cache_dir: home.join(".cache/voicectl"),
        }
    }
}

/// Capture the screen, dispatch a VznRequest over NATS, return vznd's answer.
pub struct LookAtScreen {
    cfg: VisionConfig,
    bus: Arc<voicectl_net::Bus>,
}

impl LookAtScreen {
    pub fn new(cfg: VisionConfig, bus: Arc<voicectl_net::Bus>) -> Self {
        Self { cfg, bus }
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
        "Capture the current screen and ask a vision model about it via vznd. \
         Use when the user asks what's on their screen, what an error message \
         says, what's in a video they're watching, or where to click for X. \
         The vision model handles the image — you just need to ask it the right \
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

        // 3. Dispatch over NATS and wait for vznd's reply.
        let bus = Arc::clone(&self.bus);
        let answer = block_on(async move { vzn_dispatch(&bus, b64, question).await })?;

        Ok(truncate_chars(&answer, ANSWER_CHAR_CAP))
    }
}

/// Publish a VznRequest to `vzn.request` and wait for the VznReply.
async fn vzn_dispatch(
    bus: &voicectl_net::Bus,
    image_b64: String,
    prompt: String,
) -> Result<String, String> {
    let request_id = Uuid::new_v4().to_string();
    let reply_subject = vzn_core::wire::subjects::reply_for(&request_id);

    // Subscribe BEFORE publishing to avoid a race where vznd replies before
    // we're listening.
    let mut sub = bus
        .client
        .subscribe(reply_subject.clone())
        .await
        .map_err(|e| format!("subscribe {reply_subject}: {e}"))?;

    let vzn_req = VznRequest {
        request_id: request_id.clone(),
        reply_to: reply_subject.clone(),
        prompt,
        image: ImageRef::Base64 {
            data: image_b64,
            mime: "image/png".into(),
        },
        model: None,
        max_tokens: 200,
    };

    let payload =
        serde_json::to_vec(&vzn_req).map_err(|e| format!("encode VznRequest: {e}"))?;
    bus.client
        .publish(vzn_core::wire::subjects::VZN_REQUEST, payload.into())
        .await
        .map_err(|e| format!("publish vzn.request: {e}"))?;

    debug!(request_id = %request_id, "look_at_screen dispatched to vznd");

    match tokio::time::timeout(VZN_TIMEOUT, sub.next()).await {
        Ok(Some(msg)) => {
            let reply: VznReply = serde_json::from_slice(&msg.payload)
                .map_err(|e| format!("decode VznReply: {e}"))?;
            if reply.ok {
                Ok(reply.text)
            } else {
                Err(format!(
                    "vznd returned error: {}",
                    reply.error.unwrap_or_else(|| "unknown".into())
                ))
            }
        }
        Ok(None) => Err("vzn reply subscription closed unexpectedly".into()),
        Err(_) => Err(format!(
            "look_at_screen timed out after {}s waiting for vznd",
            VZN_TIMEOUT.as_secs()
        )),
    }
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
        assert_eq!(encoded, "iVBORw0KGgo=");
    }

    #[test]
    fn vision_config_default_uses_cache_dir() {
        let cfg = VisionConfig::default();
        assert!(
            cfg.cache_dir.to_string_lossy().contains(".cache/voicectl"),
            "cache_dir={}",
            cfg.cache_dir.display()
        );
    }
}
