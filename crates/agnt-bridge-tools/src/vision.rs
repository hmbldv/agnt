//! Vision tool — `look_at_screen`.
//!
//! Captures the current desktop, base64-encodes the PNG, and dispatches a
//! [`vzn_core::wire::VznRequest`] over NATS to `vzn.request`. vznd handles
//! model selection and returns a [`vzn_core::wire::VznReply`] on the per-
//! request reply subject. Returns the reply text capped at 500 chars so it
//! stays voice-friendly.
//!
//! ## Modes
//!
//! - **Standard** (default) — single-pass: resize, encode, ask.
//! - **Enhanced** — two-pass: thumbnail → bbox localisation → crop + re-encode
//!   at higher quality → analyse. Better for reading fine text or small UI
//!   elements. Enable by passing `"enhance": true` in the tool arguments.
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

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use image::imageops::FilterType;
use image::GenericImageView;

use futures::StreamExt;
use serde::Deserialize;
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
/// How long to wait for vznd to reply (standard pass).
const VZN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for vznd on the enhanced localise pass (shorter — just a bbox).
const VZN_LOCATE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for vznd on the enhanced analyse pass.
const VZN_ANALYSE_TIMEOUT: Duration = Duration::from_secs(40);
/// Screenshot capture ceiling.
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(15);

// ─── Mode ──────────────────────────────────────────────────────────────────

pub enum VisionMode {
    Standard,
    Enhanced,
}

// ─── Bounding box (returned by the localise pass) ──────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct BBox {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

// ─── Config ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct VisionConfig {
    /// Where transient screenshots live before they're sent. Same dir the
    /// `screenshot` tool uses; files are kept (not deleted) so the user can
    /// inspect what vznd saw.
    pub cache_dir: PathBuf,
    /// Model to use for enhanced pass-1 (spatial localisation / bbox). When
    /// `None`, vznd uses its own configured default (currently `qwen2-vl-2b`).
    /// Set to e.g. `Some("qwen2-vl-2b".into())` to pin a fast model for bbox.
    pub localize_model: Option<String>,
    /// Model to use for standard single-pass and enhanced pass-2 (analysis).
    /// When `None`, vznd uses its own configured default.
    /// Set to e.g. `Some("gemma4-quality".into())` for higher-quality output.
    pub analyze_model: Option<String>,
}

impl Default for VisionConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        Self {
            cache_dir: home.join(".cache/voicectl"),
            localize_model: None,
            analyze_model: None,
        }
    }
}

// ─── Tool struct ───────────────────────────────────────────────────────────

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
         Pass enhance=true for a two-pass zoom-in that reads fine text or small \
         UI elements more accurately (costs ~2× latency). \
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
                },
                "enhance": {
                    "type": "boolean",
                    "description": "When true, uses two-pass enhanced mode: first \
                                    localises the most relevant region with a \
                                    thumbnail, then re-analyses it at full \
                                    resolution crop. Better for reading small \
                                    text, code, or error messages. Adds ~20-30 s \
                                    latency. Default: false."
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

        let enhance = args
            .get("enhance")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mode = if enhance {
            VisionMode::Enhanced
        } else {
            VisionMode::Standard
        };

        // 1. Capture screenshot to disk.
        let dir = self.screenshots_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let path = dir.join(format!("look-{ts}.png"));
        capture_screen(&path)?;

        // 2. Read raw PNG bytes.
        let png_bytes =
            std::fs::read(&path).map_err(|e| format!("read screenshot {}: {e}", path.display()))?;

        // 3. Dispatch — mode-dependent.
        let bus = Arc::clone(&self.bus);
        let localize_model = self.cfg.localize_model.clone();
        let analyze_model = self.cfg.analyze_model.clone();
        let answer = match mode {
            VisionMode::Standard => {
                let (jpeg_bytes, mime) =
                    resize_screenshot(&png_bytes).map_err(|e| format!("resize screenshot: {e}"))?;
                let b64 = base64_encode(&jpeg_bytes);
                block_on(async move {
                    vzn_dispatch_standard(
                        &bus,
                        b64,
                        mime,
                        question,
                        analyze_model.as_deref(),
                    ).await
                })?
            }
            VisionMode::Enhanced => {
                // Capture dimensions from the raw PNG before handing bytes off.
                let img = image::load_from_memory(&png_bytes)
                    .map_err(|e| format!("decode screenshot: {e}"))?;
                let (orig_w, orig_h) = img.dimensions();
                drop(img); // release before cloning into async

                block_on(async move {
                    vzn_dispatch_enhanced(
                        &bus,
                        vzn_core::wire::subjects::VZN_REQUEST,
                        &png_bytes,
                        orig_w,
                        orig_h,
                        &question,
                        localize_model.as_deref(),
                        analyze_model.as_deref(),
                    )
                    .await
                })?
            }
        };

        Ok(truncate_chars(&answer, ANSWER_CHAR_CAP))
    }
}

// ─── NATS helpers ──────────────────────────────────────────────────────────

/// Publish a VznRequest and wait for the VznReply — shared low-level primitive.
///
/// `model` overrides vznd's configured default when `Some`. Pass `None` to let
/// vznd decide (it uses whatever `[inference].model` is in `/etc/vzn/config.toml`).
async fn vzn_roundtrip(
    bus: &voicectl_net::Bus,
    b64: String,
    mime: String,
    prompt: String,
    model: Option<&str>,
    max_tokens: u32,
    timeout: Duration,
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
            data: b64,
            mime,
        },
        model: model.map(|s| s.to_string()),
        max_tokens,
    };

    let payload =
        serde_json::to_vec(&vzn_req).map_err(|e| format!("encode VznRequest: {e}"))?;
    bus.client
        .publish(vzn_core::wire::subjects::VZN_REQUEST, payload.into())
        .await
        .map_err(|e| format!("publish vzn.request: {e}"))?;

    debug!(request_id = %request_id, max_tokens, "vzn_roundtrip dispatched");

    match tokio::time::timeout(timeout, sub.next()).await {
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
            timeout.as_secs()
        )),
    }
}

/// Standard single-pass dispatch.
///
/// `analyze_model` — when `Some`, overrides vznd's default model for this call.
async fn vzn_dispatch_standard(
    bus: &voicectl_net::Bus,
    image_b64: String,
    mime: &'static str,
    prompt: String,
    analyze_model: Option<&str>,
) -> Result<String, String> {
    vzn_roundtrip(bus, image_b64, mime.to_string(), prompt, analyze_model, 200, VZN_TIMEOUT).await
}

/// Enhanced two-pass dispatch:
///
/// Pass 1 — thumbnail → locate bounding box of most relevant region.
/// Pass 2 — crop original at that bbox, resize to 1024, re-encode at higher
///           quality, then analyse with the real prompt.
///
/// `localize_model` — model override for pass-1 bbox localisation. When `None`,
/// vznd uses its configured default. A fast model (e.g. `qwen2-vl-2b`) works
/// well here since we only need a coarse bbox.
///
/// `analyze_model` — model override for pass-2 analysis. When `None`, vznd
/// uses its configured default. A quality model (e.g. `gemma4-quality`) yields
/// better text-reading results.
async fn vzn_dispatch_enhanced(
    bus: &voicectl_net::Bus,
    _subject: &str, // kept for symmetry; vzn_roundtrip always uses VZN_REQUEST
    png_bytes: &[u8],
    orig_w: u32,
    orig_h: u32,
    prompt: &str,
    localize_model: Option<&str>,
    analyze_model: Option<&str>,
) -> Result<String, String> {
    // ── Pass 1: localise ──────────────────────────────────────────────────
    let img = image::load_from_memory(png_bytes)
        .map_err(|e| format!("enhanced: decode png: {e}"))?;

    let thumb = resize_to_max(&img, 1024);
    let thumb_bytes =
        encode_jpeg(&thumb, 75).map_err(|e| format!("enhanced: encode thumb: {e}"))?;
    let thumb_b64 = base64_encode(&thumb_bytes);

    let locate_prompt = format!(
        "Return ONLY a JSON object with the bounding box of the most relevant \
         content area in the original image coordinates ({}x{}). \
         Format: {{\"x\":N,\"y\":N,\"w\":N,\"h\":N}}. No other text.",
        orig_w, orig_h
    );

    let loc_reply = vzn_roundtrip(
        bus,
        thumb_b64,
        "image/jpeg".to_string(),
        locate_prompt,
        localize_model,
        60,
        VZN_LOCATE_TIMEOUT,
    )
    .await
    .unwrap_or_default();

    // Parse bbox — fall back to full image on any failure.
    let bbox = parse_bbox(&loc_reply, orig_w, orig_h).unwrap_or(BBox {
        x: 0,
        y: 0,
        w: orig_w,
        h: orig_h,
    });

    debug!("enhanced mode bbox: {:?}", bbox);

    // Coverage guard: if bbox covers > 60% of the full frame, pass-2 won't
    // add value — fall back to a standard single-pass on the full image.
    let bbox_area = bbox.w as u64 * bbox.h as u64;
    let full_area = orig_w as u64 * orig_h as u64;
    if bbox_area > (full_area * 6 / 10) {
        tracing::debug!(
            "enhanced bbox covers {:.0}% of frame — falling back to standard",
            bbox_area as f64 / full_area as f64 * 100.0
        );
        let (jpeg_bytes, _mime) =
            resize_screenshot(png_bytes).map_err(|e| format!("enhanced fallback resize: {e}"))?;
        let b64 = base64_encode(&jpeg_bytes);
        return vzn_roundtrip(
            bus,
            b64,
            "image/jpeg".to_string(),
            prompt.to_string(),
            analyze_model,
            200,
            VZN_TIMEOUT,
        )
        .await;
    }

    // ── Pass 2: crop + analyse ────────────────────────────────────────────
    let crop = img.crop_imm(bbox.x, bbox.y, bbox.w, bbox.h);
    let crop_resized = resize_to_max(&crop, 1024);
    let crop_bytes =
        encode_jpeg(&crop_resized, 85).map_err(|e| format!("enhanced: encode crop: {e}"))?;
    let crop_b64 = base64_encode(&crop_bytes);

    vzn_roundtrip(
        bus,
        crop_b64,
        "image/jpeg".to_string(),
        prompt.to_string(),
        analyze_model,
        1024,
        VZN_ANALYSE_TIMEOUT,
    )
    .await
}

// ─── Image helpers ─────────────────────────────────────────────────────────

/// Resize `img` so its width is at most `max_width`, preserving aspect ratio.
/// Returns a clone if already within bounds.
fn resize_to_max(img: &image::DynamicImage, max_width: u32) -> image::DynamicImage {
    let (w, _h) = img.dimensions();
    if w <= max_width {
        img.clone()
    } else {
        img.resize(max_width, u32::MAX, FilterType::Triangle)
    }
}

/// Encode `img` as JPEG at the given quality (0–100). Returns raw JPEG bytes.
fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut cursor = Cursor::new(&mut buf);
    let encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, quality);
    img.write_with_encoder(encoder)?;
    Ok(buf)
}

/// Load PNG bytes, scale down to max 1024 px wide if needed, and re-encode as
/// JPEG at quality 75. Returns the JPEG bytes and the MIME type string.
///
/// Kept for backwards-compatibility (used by Standard mode and tests).
fn resize_screenshot(png_bytes: &[u8]) -> anyhow::Result<(Vec<u8>, &'static str)> {
    let img = image::load_from_memory(png_bytes)?;
    let resized = resize_to_max(&img, 1024);
    let jpeg = encode_jpeg(&resized, 75)?;
    Ok((jpeg, "image/jpeg"))
}

/// Scan `reply` for the first JSON object and try to deserialise it as a
/// [`BBox`]. Clamps all fields to the given image dimensions. Returns `None`
/// on any parse failure so the caller can fall back gracefully.
fn parse_bbox(reply: &str, max_w: u32, max_h: u32) -> Option<BBox> {
    let start = reply.find('{')?;
    let end = reply.rfind('}')? + 1;
    let bbox: BBox = serde_json::from_str(&reply[start..end]).ok()?;

    // Clamp to image bounds.
    let x = bbox.x.min(max_w.saturating_sub(1));
    let y = bbox.y.min(max_h.saturating_sub(1));
    let w = bbox.w.min(max_w - x).max(1);
    let h = bbox.h.min(max_h - y).max(1);
    Some(BBox { x, y, w, h })
}

// ─── Screen capture ────────────────────────────────────────────────────────

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

// ─── Misc ──────────────────────────────────────────────────────────────────

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

// ─── Tests ─────────────────────────────────────────────────────────────────

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

    #[test]
    fn parse_bbox_valid() {
        let reply = r#"{"x":100,"y":200,"w":400,"h":300}"#;
        let bbox = parse_bbox(reply, 1920, 1080).unwrap();
        assert_eq!(bbox.x, 100);
        assert_eq!(bbox.y, 200);
        assert_eq!(bbox.w, 400);
        assert_eq!(bbox.h, 300);
    }

    #[test]
    fn parse_bbox_with_surrounding_text() {
        let reply = r#"Here is the bounding box: {"x":10,"y":20,"w":500,"h":400} as requested."#;
        let bbox = parse_bbox(reply, 1920, 1080).unwrap();
        assert_eq!(bbox.x, 10);
        assert_eq!(bbox.y, 20);
        assert_eq!(bbox.w, 500);
        assert_eq!(bbox.h, 400);
    }

    #[test]
    fn parse_bbox_clamps_to_image_bounds() {
        // w + x would exceed max_w
        let reply = r#"{"x":1800,"y":0,"w":500,"h":100}"#;
        let bbox = parse_bbox(reply, 1920, 1080).unwrap();
        // x clamped to max_w-1 = 1919, w clamped to max_w - x = 1
        assert!(bbox.x < 1920);
        assert!(bbox.x + bbox.w <= 1920);
    }

    #[test]
    fn parse_bbox_returns_none_on_invalid_json() {
        assert!(parse_bbox("no json here", 1920, 1080).is_none());
        assert!(parse_bbox("{bad}", 1920, 1080).is_none());
    }

    #[test]
    fn resize_to_max_passthrough_when_small() {
        // Build a small synthetic image.
        let img = image::DynamicImage::new_rgb8(800, 600);
        let resized = resize_to_max(&img, 1024);
        assert_eq!(resized.width(), 800);
    }

    #[test]
    fn resize_to_max_shrinks_large_image() {
        let img = image::DynamicImage::new_rgb8(2048, 1536);
        let resized = resize_to_max(&img, 1024);
        assert!(resized.width() <= 1024, "width {} > 1024", resized.width());
    }

    #[test]
    fn encode_jpeg_produces_valid_jpeg() {
        let img = image::DynamicImage::new_rgb8(64, 64);
        let bytes = encode_jpeg(&img, 75).unwrap();
        assert!(!bytes.is_empty());
        // JPEG magic bytes: FF D8
        assert_eq!(bytes[0], 0xFF);
        assert_eq!(bytes[1], 0xD8);
    }

    #[test]
    fn resize_screenshot_reduces_large_png() {
        use std::fs;
        // find any screenshot in the cache
        let dir = dirs::home_dir().unwrap().join(".cache/voicectl/screenshots");
        let png = fs::read_dir(&dir).ok().and_then(|mut e| {
            e.find_map(|f| {
                let f = f.ok()?;
                let p = f.path();
                if p.extension()?.to_str()? == "png" { Some(p) } else { None }
            })
        });
        if let Some(png_path) = png {
            let bytes = fs::read(&png_path).unwrap();
            let (jpeg, mime) = resize_screenshot(&bytes).unwrap();
            assert_eq!(mime, "image/jpeg");
            assert!(!jpeg.is_empty());
            // verify dimensions via image crate
            let img = image::load_from_memory(&jpeg).unwrap();
            assert!(img.width() <= 1024, "width {} > 1024", img.width());
            eprintln!("Resized to {}x{}, JPEG {} bytes", img.width(), img.height(), jpeg.len());
        }
    }
}
