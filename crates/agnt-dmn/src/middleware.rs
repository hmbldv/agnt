//! HTTP middleware for the daemon.

use axum::{
    extract::State,
    http::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Constant-time byte-slice equality to prevent timing attacks on token comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    // XOR all byte pairs and OR the differences together — any mismatch sets a bit.
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Axum middleware that requires a valid `Authorization: Bearer <token>` header.
/// Returns 401 for missing, malformed, or incorrect tokens.
pub async fn require_auth(
    State(token): State<String>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let provided = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    let ok = provided
        .map(|t| constant_time_eq(t.as_bytes(), token.as_bytes()))
        .unwrap_or(false);

    if !ok {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    next.run(req).await
}
