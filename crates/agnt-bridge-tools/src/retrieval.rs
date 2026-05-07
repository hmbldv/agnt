//! Semantic retrieval tools — `semantic_search` and `rerank` — backed by
//! LiteLLM (nomic-embed-text for embeddings, bge-reranker for reranking).
//!
//! Both tools hit the LiteLLM proxy at a configurable base URL (default:
//! `http://100.80.135.46:4000/v1`). They are intentionally independent so
//! the agent can call either one without the other.
//!
//! ## API shapes
//!
//! **Embeddings** — POST `/v1/embeddings`
//! ```json
//! { "model": "nomic-embed-text", "input": "search_query: <text>" }
//! // Response: { "data": [{ "embedding": [...768 floats] }] }
//! ```
//!
//! **Rerank** — POST `/v1/rerank`
//! ```json
//! { "model": "bge-reranker", "query": "...", "documents": ["..."] }
//! // Response: { "results": [{ "index": N, "relevance_score": 0.99 }] }
//! ```

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::shell::block_on;

// ─────────────────────────────────────────────────────────────────────────────
// Config
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration shared by the retrieval tools.
#[derive(Clone, Debug)]
pub struct RetrievalConfig {
    /// LiteLLM proxy base URL. Default: `http://100.80.135.46:4000/v1`.
    pub litellm_url: String,
    /// Embedding model name. Default: `nomic-embed-text`.
    pub embed_model: String,
    /// Reranker model name. Default: `bge-reranker`.
    pub rerank_model: String,
    /// Environment variable name to read the API key from at call time.
    /// Default: `"LITELLM_API_KEY"`. The value is read on every tool call
    /// (not cached at config time) so a service restart is not needed when
    /// the key rotates. If the env var is unset, requests are sent without
    /// an `Authorization` header — appropriate for unauthenticated proxies.
    pub api_key_env: String,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            litellm_url: "http://100.80.135.46:4000/v1".into(),
            embed_model: "nomic-embed-text".into(),
            rerank_model: "bge-reranker".into(),
            api_key_env: "LITELLM_API_KEY".into(),
        }
    }
}

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DOCUMENTS: usize = 20;

// ─────────────────────────────────────────────────────────────────────────────
// SemanticSearch
// ─────────────────────────────────────────────────────────────────────────────

/// Embed query + documents via nomic-embed-text, rank by cosine similarity,
/// return top-k.
///
/// Uses two embedding calls: one for the query (prefix `"search_query: "`),
/// one batch call for the documents (prefix `"search_document: "`).
pub struct SemanticSearch {
    cfg: RetrievalConfig,
}

impl SemanticSearch {
    pub fn new(cfg: RetrievalConfig) -> Self {
        Self { cfg }
    }
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedItem>,
}

#[derive(Debug, Deserialize)]
struct EmbedItem {
    embedding: Vec<f32>,
}

impl agnt::Tool for SemanticSearch {
    fn name(&self) -> &str {
        "semantic_search"
    }

    fn description(&self) -> &str {
        "Embed a query and a list of text passages using nomic-embed-text, \
         then rank the passages by cosine similarity to the query. Returns \
         the top-k most relevant passages with their similarity scores. Use \
         to find semantically relevant context from a known corpus — memory \
         snippets, document chunks, log entries, etc."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query."
                },
                "documents": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Passages to search against (max 20).",
                    "minItems": 1,
                    "maxItems": 20
                },
                "top_k": {
                    "type": "integer",
                    "description": "Number of top results to return (default 3, max 20).",
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query", "documents"]
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

        let documents: Vec<String> = args
            .get("documents")
            .and_then(|v| v.as_array())
            .ok_or("missing 'documents' (array of strings)")?
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or("each document must be a string")
                    .map(|s| s.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;

        if documents.is_empty() {
            return Err("documents must not be empty".into());
        }
        if documents.len() > MAX_DOCUMENTS {
            return Err(format!("at most {MAX_DOCUMENTS} documents allowed"));
        }

        let top_k = args
            .get("top_k")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(3)
            .clamp(1, documents.len());

        let cfg = self.cfg.clone();
        let result = block_on(async move {
            semantic_search_inner(&cfg, &query, &documents, top_k).await
        })?;

        Ok(result)
    }
}

/// Read the API key from the configured env var. Returns `None` if unset.
fn read_api_key(cfg: &RetrievalConfig) -> Option<String> {
    std::env::var(&cfg.api_key_env).ok().filter(|s| !s.is_empty())
}

async fn embed_batch(cfg: &RetrievalConfig, inputs: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| format!("build http client: {e}"))?;

    let url = format!("{}/embeddings", cfg.litellm_url.trim_end_matches('/'));
    let body = json!({
        "model": cfg.embed_model,
        "input": inputs
    });

    let mut req = client.post(&url).json(&body);
    if let Some(key) = read_api_key(cfg) {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read embeddings response: {e}"))?;

    if !status.is_success() {
        return Err(format!(
            "LiteLLM embeddings returned {} — {}",
            status,
            text.chars().take(300).collect::<String>()
        ));
    }

    let parsed: EmbedResponse =
        serde_json::from_str(&text).map_err(|e| format!("decode embeddings response: {e}"))?;

    Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

async fn semantic_search_inner(
    cfg: &RetrievalConfig,
    query: &str,
    documents: &[String],
    top_k: usize,
) -> Result<String, String> {
    // Build all inputs in one batch: query first, then documents.
    let query_input = format!("search_query: {query}");
    let doc_inputs: Vec<String> = documents
        .iter()
        .map(|d| format!("search_document: {d}"))
        .collect();

    let mut all_inputs = vec![query_input];
    all_inputs.extend(doc_inputs);

    let mut embeddings = embed_batch(cfg, all_inputs).await?;

    if embeddings.len() != documents.len() + 1 {
        return Err(format!(
            "expected {} embeddings, got {}",
            documents.len() + 1,
            embeddings.len()
        ));
    }

    let query_emb = embeddings.remove(0);
    let mut scored: Vec<(usize, f32)> = embeddings
        .iter()
        .enumerate()
        .map(|(i, emb)| (i, cosine_similarity(&query_emb, emb)))
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut out = format!("Top {top_k} results (cosine similarity):\n");
    for (rank, (idx, score)) in scored.into_iter().take(top_k).enumerate() {
        out.push_str(&format!(
            "\n{}. [score: {:.4}] {}\n",
            rank + 1,
            score,
            documents[idx]
        ));
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// Rerank
// ─────────────────────────────────────────────────────────────────────────────

/// Rerank a list of passages against a query using bge-reranker via LiteLLM.
pub struct Rerank {
    cfg: RetrievalConfig,
}

impl Rerank {
    pub fn new(cfg: RetrievalConfig) -> Self {
        Self { cfg }
    }
}

#[derive(Debug, Deserialize)]
struct RerankResponse {
    results: Vec<RerankItem>,
}

#[derive(Debug, Deserialize)]
struct RerankItem {
    index: usize,
    relevance_score: f64,
}

impl agnt::Tool for Rerank {
    fn name(&self) -> &str {
        "rerank"
    }

    fn description(&self) -> &str {
        "Rerank a list of text passages for a query using bge-reranker. \
         Returns passages sorted by relevance score descending. More \
         efficient than semantic_search for small corpora — single API call \
         instead of N+1. Use when you already have candidate passages and \
         want to surface the most relevant ones."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The query to rank passages against."
                },
                "passages": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Passages to rerank (max 20).",
                    "minItems": 1,
                    "maxItems": 20
                },
                "top_k": {
                    "type": "integer",
                    "description": "Number of top passages to return (default 5, max 20).",
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query", "passages"]
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

        let passages: Vec<String> = args
            .get("passages")
            .and_then(|v| v.as_array())
            .ok_or("missing 'passages' (array of strings)")?
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or("each passage must be a string")
                    .map(|s| s.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;

        if passages.is_empty() {
            return Err("passages must not be empty".into());
        }
        if passages.len() > MAX_DOCUMENTS {
            return Err(format!("at most {MAX_DOCUMENTS} passages allowed"));
        }

        let top_k = args
            .get("top_k")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(5)
            .clamp(1, passages.len());

        let cfg = self.cfg.clone();
        let result =
            block_on(async move { rerank_inner(&cfg, &query, &passages, top_k).await })?;

        Ok(result)
    }
}

async fn rerank_inner(
    cfg: &RetrievalConfig,
    query: &str,
    passages: &[String],
    top_k: usize,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| format!("build http client: {e}"))?;

    let url = format!("{}/rerank", cfg.litellm_url.trim_end_matches('/'));
    let body = json!({
        "model": cfg.rerank_model,
        "query": query,
        "documents": passages
    });

    let mut req = client.post(&url).json(&body);
    if let Some(key) = read_api_key(cfg) {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read rerank response: {e}"))?;

    if !status.is_success() {
        return Err(format!(
            "LiteLLM rerank returned {} — {}",
            status,
            text.chars().take(300).collect::<String>()
        ));
    }

    let parsed: RerankResponse =
        serde_json::from_str(&text).map_err(|e| format!("decode rerank response: {e}"))?;

    // Sort by relevance descending (API may already return sorted, but be safe).
    let mut results = parsed.results;
    results.sort_by(|a, b| {
        b.relevance_score
            .partial_cmp(&a.relevance_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if results.is_empty() {
        return Ok("(no results returned by reranker)".into());
    }

    let mut out = format!("Top {top_k} passages (relevance score):\n");
    for (rank, item) in results.into_iter().take(top_k).enumerate() {
        let text = passages
            .get(item.index)
            .map(|s| s.as_str())
            .unwrap_or("(index out of range)");
        out.push_str(&format!(
            "\n{}. [score: {:.4}] {}\n",
            rank + 1,
            item.relevance_score,
            text
        ));
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use agnt::Tool;

    fn default_cfg() -> RetrievalConfig {
        RetrievalConfig::default()
    }

    // ── SemanticSearch schema / validation ───────────────────────────────────

    #[test]
    fn semantic_search_schema_is_valid_json_object() {
        let t = SemanticSearch::new(default_cfg());
        let s = t.schema();
        assert_eq!(s["type"], "object");
        assert!(s["properties"]["query"].is_object());
        assert!(s["properties"]["documents"].is_object());
        assert_eq!(s["required"][0], "query");
        assert_eq!(s["required"][1], "documents");
    }

    #[test]
    fn semantic_search_rejects_missing_query() {
        let t = SemanticSearch::new(default_cfg());
        let err = t
            .call(json!({"documents": ["a"]}))
            .unwrap_err();
        assert!(err.contains("query"), "{err}");
    }

    #[test]
    fn semantic_search_rejects_empty_query() {
        let t = SemanticSearch::new(default_cfg());
        let err = t
            .call(json!({"query": "  ", "documents": ["a"]}))
            .unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn semantic_search_rejects_missing_documents() {
        let t = SemanticSearch::new(default_cfg());
        let err = t.call(json!({"query": "hello"})).unwrap_err();
        assert!(err.contains("documents"), "{err}");
    }

    #[test]
    fn semantic_search_rejects_too_many_documents() {
        let t = SemanticSearch::new(default_cfg());
        let docs: Vec<Value> = (0..21).map(|i| json!(format!("doc {i}"))).collect();
        let err = t
            .call(json!({"query": "hello", "documents": docs}))
            .unwrap_err();
        assert!(err.contains("20"), "{err}");
    }

    #[test]
    fn semantic_search_name_and_description_are_set() {
        let t = SemanticSearch::new(default_cfg());
        assert_eq!(t.name(), "semantic_search");
        assert!(!t.description().is_empty());
    }

    // ── Rerank schema / validation ────────────────────────────────────────────

    #[test]
    fn rerank_schema_is_valid_json_object() {
        let t = Rerank::new(default_cfg());
        let s = t.schema();
        assert_eq!(s["type"], "object");
        assert!(s["properties"]["query"].is_object());
        assert!(s["properties"]["passages"].is_object());
        assert_eq!(s["required"][0], "query");
        assert_eq!(s["required"][1], "passages");
    }

    #[test]
    fn rerank_rejects_missing_query() {
        let t = Rerank::new(default_cfg());
        let err = t.call(json!({"passages": ["a"]})).unwrap_err();
        assert!(err.contains("query"), "{err}");
    }

    #[test]
    fn rerank_rejects_empty_query() {
        let t = Rerank::new(default_cfg());
        let err = t
            .call(json!({"query": "", "passages": ["a"]}))
            .unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn rerank_rejects_missing_passages() {
        let t = Rerank::new(default_cfg());
        let err = t.call(json!({"query": "hello"})).unwrap_err();
        assert!(err.contains("passages"), "{err}");
    }

    #[test]
    fn rerank_rejects_too_many_passages() {
        let t = Rerank::new(default_cfg());
        let passages: Vec<Value> = (0..21).map(|i| json!(format!("p {i}"))).collect();
        let err = t
            .call(json!({"query": "hello", "passages": passages}))
            .unwrap_err();
        assert!(err.contains("20"), "{err}");
    }

    #[test]
    fn rerank_name_and_description_are_set() {
        let t = Rerank::new(default_cfg());
        assert_eq!(t.name(), "rerank");
        assert!(!t.description().is_empty());
    }

    // ── cosine_similarity unit tests ─────────────────────────────────────────

    #[test]
    fn cosine_identical_vectors_is_one() {
        let v = vec![1.0f32, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-5, "expected ~1.0 got {sim}");
    }

    #[test]
    fn cosine_orthogonal_vectors_is_zero() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6, "expected ~0.0 got {sim}");
    }

    #[test]
    fn cosine_empty_vector_returns_zero() {
        let sim = cosine_similarity(&[], &[]);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn cosine_mismatched_lengths_returns_zero() {
        let sim = cosine_similarity(&[1.0f32], &[1.0f32, 2.0]);
        assert_eq!(sim, 0.0);
    }

    // ── RetrievalConfig defaults ──────────────────────────────────────────────

    #[test]
    fn config_defaults_are_sensible() {
        let cfg = RetrievalConfig::default();
        assert!(cfg.litellm_url.contains("4000"));
        assert_eq!(cfg.embed_model, "nomic-embed-text");
        assert_eq!(cfg.rerank_model, "bge-reranker");
    }
}
