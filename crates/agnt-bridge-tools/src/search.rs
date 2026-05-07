//! Web search tool — backed by SearXNG's JSON API.
//!
//! Hits `<searxng_url>/search?q=<query>&format=json` and renders the top-N
//! results as a numbered list the LLM can read directly.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::shell::block_on;

#[derive(Clone, Debug)]
pub struct SearchConfig {
    pub searxng_url: String,
}

/// Web search via a SearXNG instance.
pub struct WebSearch {
    cfg: SearchConfig,
}

impl WebSearch {
    pub fn new(cfg: SearchConfig) -> Self {
        Self { cfg }
    }
}

#[derive(Debug, Deserialize)]
struct SxResponse {
    #[serde(default)]
    results: Vec<SxResult>,
}

#[derive(Debug, Deserialize)]
struct SxResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

const DEFAULT_MAX_RESULTS: usize = 5;
const SNIPPET_CAP: usize = 140;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

impl agnt::Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the open web via the user's SearXNG instance. Returns a \
         numbered list of titles, URLs, and short snippets. Use whenever the \
         user asks about something you don't already know — current events, \
         documentation, prices, etc."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query — natural language is fine."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default 5, max 10).",
                    "minimum": 1,
                    "maximum": 10
                }
            },
            "required": ["query"]
        })
    }

    fn call(&self, args: Value) -> Result<String, String> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or("missing 'query' (string)")?
            .trim()
            .to_string();
        if query.is_empty() {
            return Err("query must not be empty".into());
        }
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, 10);

        let body = block_on(fetch_searxng(&self.cfg.searxng_url, &query))?;
        let parsed: SxResponse =
            serde_json::from_str(&body).map_err(|e| format!("decode SearXNG response: {e}"))?;
        Ok(render_results(&parsed.results, max_results))
    }
}

async fn fetch_searxng(base: &str, query: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        // SearXNG sometimes returns 4xx for default UA — set an explicit one.
        .user_agent("agnt-bridge-tools/0.1 (+https://github.com/hmbldv/voicectl)")
        .build()
        .map_err(|e| format!("build http client: {e}"))?;
    let url = format!(
        "{}/search?q={}&format=json",
        base.trim_end_matches('/'),
        urlencoding(query)
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("read response body: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "SearXNG returned {} — first 200 chars: {}",
            status,
            body.chars().take(200).collect::<String>()
        ));
    }
    Ok(body)
}

/// Minimal URL form-encoding for the query parameter — we only encode
/// what's needed to round-trip arbitrary search queries.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{:02X}", other)),
        }
    }
    out
}

fn render_results(results: &[SxResult], cap: usize) -> String {
    if results.is_empty() {
        return "No results.".to_string();
    }
    let mut out = String::new();
    for (i, r) in results.iter().take(cap).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let title = if r.title.is_empty() {
            "(untitled)".to_string()
        } else {
            r.title.clone()
        };
        let snippet = trim_snippet(&r.content, SNIPPET_CAP);
        out.push_str(&format!("{}. {} — {}\n   {}", i + 1, title, r.url, snippet));
    }
    out
}

fn trim_snippet(s: &str, cap: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= cap {
        return collapsed;
    }
    let mut out: String = collapsed.chars().take(cap).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_empty_returns_no_results_marker() {
        assert_eq!(render_results(&[], 5), "No results.");
    }

    #[test]
    fn render_truncates_to_cap() {
        let mk = |n: usize| SxResult {
            title: format!("T{n}"),
            url: format!("https://example.com/{n}"),
            content: "snippet".into(),
        };
        let results: Vec<SxResult> = (0..10).map(mk).collect();
        let rendered = render_results(&results, 3);
        assert!(rendered.contains("1. T0"));
        assert!(rendered.contains("3. T2"));
        assert!(!rendered.contains("4. T3"));
    }

    #[test]
    fn render_handles_long_snippet() {
        let r = SxResult {
            title: "Title".into(),
            url: "https://x".into(),
            content: "word ".repeat(100),
        };
        let out = render_results(&[r], 1);
        assert!(out.contains('…'));
    }

    #[test]
    fn parser_accepts_real_searxng_shape() {
        // Captured from a live SearXNG /search?...&format=json response.
        let body = r#"{
            "query": "rust",
            "results": [
                {
                    "url": "https://www.rust-lang.org/",
                    "title": "Rust Programming Language",
                    "content": "A language empowering everyone to build reliable and efficient software."
                },
                {
                    "url": "https://doc.rust-lang.org/",
                    "title": "The Rust Programming Language",
                    "content": "Rust documentation."
                }
            ]
        }"#;
        let parsed: SxResponse = serde_json::from_str(body).expect("parse");
        assert_eq!(parsed.results.len(), 2);
        assert_eq!(parsed.results[0].title, "Rust Programming Language");
        let rendered = render_results(&parsed.results, 5);
        assert!(rendered.contains("Rust Programming Language"));
        assert!(rendered.contains("https://www.rust-lang.org/"));
    }

    #[test]
    fn parser_handles_empty_results() {
        let body = r#"{ "query": "no-hits-zzz", "results": [] }"#;
        let parsed: SxResponse = serde_json::from_str(body).unwrap();
        assert!(parsed.results.is_empty());
    }

    #[test]
    fn url_encoding_roundtrip() {
        assert_eq!(urlencoding("rust lang"), "rust+lang");
        assert_eq!(urlencoding("c++"), "c%2B%2B");
        assert_eq!(urlencoding("hello/world?"), "hello%2Fworld%3F");
    }

    // Live integration test — requires SearXNG reachable at the configured URL.
    // Run with: cargo test -p agnt-bridge-tools --test … -- --ignored
    #[tokio::test]
    #[ignore = "requires live SearXNG at http://lnx-rig:8888"]
    async fn live_searxng_returns_results() {
        let body = fetch_searxng("http://lnx-rig:8888", "rust programming language")
            .await
            .expect("fetch");
        let parsed: SxResponse = serde_json::from_str(&body).expect("decode");
        assert!(!parsed.results.is_empty(), "no results returned");
    }
}
