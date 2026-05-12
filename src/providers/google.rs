//! Google provider — Search via Gemini API with Google Search grounding.
//!
//! Reference: https://ai.google.dev/gemini-api/docs/grounding
//!
//! Uses the Gemini generateContent endpoint with `tools: [{ google_search: {} }]`
//! to search the web. No Custom Search Engine ID (cx) needed — the grounding
//! is built into the Gemini API itself.
//!
//! Endpoint: {base_url}/models/{model}:generateContent?key={api_key}
//!
//! Config example:
//!   [providers.google]
//!   api_key = "YOUR_GEMINI_API_KEY"
//!
//!   [providers.google.search]
//!   base_url = "https://generativelanguage.googleapis.com/v1beta"
//!
//!   [providers.google.search.models."gemini-2.0-flash"]

use crate::providers::{SearchProvider, SearchRequest, SearchResult, SearchResults};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_MODEL: &str = "gemini-2.0-flash";

#[derive(Clone)]
pub struct GoogleProvider {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl GoogleProvider {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            base_url,
            api_key,
            client: reqwest::Client::new(),
        }
    }
}

// ── SearchProvider ────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct GeminiResponse {
    candidates: Option<Vec<Candidate>>,
    error: Option<GeminiError>,
}

#[derive(serde::Deserialize)]
struct Candidate {
    content: Option<Content>,
    #[serde(rename = "groundingMetadata")]
    grounding_metadata: Option<GroundingMetadata>,
}

#[derive(serde::Deserialize)]
struct Content {
    parts: Option<Vec<Part>>,
}

#[derive(serde::Deserialize)]
struct Part {
    text: Option<String>,
}

#[derive(serde::Deserialize)]
struct GroundingMetadata {
    #[serde(rename = "groundingChunks")]
    grounding_chunks: Option<Vec<GroundingChunk>>,
    #[serde(rename = "groundingSupports")]
    #[allow(dead_code)]
    grounding_supports: Option<Vec<GroundingSupport>>,
}

#[derive(serde::Deserialize)]
struct GroundingChunk {
    web: Option<WebChunk>,
}

#[derive(serde::Deserialize)]
struct WebChunk {
    uri: Option<String>,
    title: Option<String>,
}

#[derive(serde::Deserialize)]
struct GroundingSupport {
    #[serde(rename = "groundingChunkIndices")]
    #[allow(dead_code)]
    grounding_chunk_indices: Option<Vec<u64>>,
    #[allow(dead_code)]
    segment: Option<Segment>,
}

#[derive(serde::Deserialize)]
struct Segment {
    #[allow(dead_code)]
    text: Option<String>,
}

#[derive(serde::Deserialize)]
struct GeminiError {
    code: Option<u32>,
    message: Option<String>,
    status: Option<String>,
}

impl SearchProvider for GoogleProvider {
    fn search(&self, req: SearchRequest) -> anyhow::Result<SearchResults> {
        let model = req
            .search_type
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_MODEL);

        let url = format!(
            "{}/models/{}:generateContent?key={}",
            self.base_url.trim_end_matches('/'),
            model,
            self.api_key,
        );

        let body = serde_json::json!({
            "contents": [{
                "parts": [{
                    "text": req.query
                }]
            }],
            "tools": [{
                "google_search": {}
            }]
        });

        let resp_text = futures::executor::block_on(async {
            let resp = self
                .client
                .post(&url)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await?;

            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("Google Gemini HTTP {}: {}", status, text);
            }
            resp.text().await.map_err(|e| anyhow::anyhow!(e.to_string()))
        })?;

        let resp: GeminiResponse = serde_json::from_str(&resp_text)?;

        // Debug: log grounding metadata state.
        if let Some(ref candidate) = resp.candidates.as_ref().and_then(|c| c.first()) {
            let has_meta = candidate.grounding_metadata.is_some();
            let chunk_count = candidate.grounding_metadata.as_ref()
                .and_then(|m| m.grounding_chunks.as_ref())
                .map(|c| c.len())
                .unwrap_or(0);
            let support_count = candidate.grounding_metadata.as_ref()
                .and_then(|m| m.grounding_supports.as_ref())
                .map(|s| s.len())
                .unwrap_or(0);
            tracing::info!(
                has_meta,
                chunk_count,
                support_count,
                "Google Gemini grounding metadata"
            );
        }

        if let Some(err) = resp.error {
            let msg = err.message.unwrap_or_else(|| err.status.unwrap_or_default());
            anyhow::bail!("Google Gemini error {}: {}", err.code.unwrap_or(0), msg);
        }

        let candidate = match resp.candidates.and_then(|mut c| c.pop()) {
            Some(c) => c,
            None => {
                return Ok(SearchResults {
                    results: vec![],
                    total: Some(0),
                    query: req.query,
                });
            }
        };

        // Extract text content from the response.
        let _content = candidate
            .content
            .as_ref()
            .and_then(|c| c.parts.as_ref())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.text.as_deref())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        // Extract citations from grounding metadata.
        let mut results: Vec<SearchResult> = Vec::new();
        if let Some(meta) = candidate.grounding_metadata {
            // Collect per-chunk snippet text from groundingSupports.
            // Each support maps a text segment to one or more chunk indices.
            let mut chunk_snippets: std::collections::HashMap<usize, String> =
                std::collections::HashMap::new();
            if let Some(supports) = meta.grounding_supports.as_ref() {
                for support in supports {
                    let seg = support
                        .segment
                        .as_ref()
                        .and_then(|s| s.text.as_deref())
                        .unwrap_or("")
                        .trim();
                    if seg.is_empty() {
                        continue;
                    }
                    if let Some(indices) = &support.grounding_chunk_indices {
                        for &idx in indices {
                            chunk_snippets
                                .entry(idx as usize)
                                .and_modify(|e| {
                                    if !e.contains(seg) {
                                        e.push_str("; ");
                                        e.push_str(seg);
                                    }
                                })
                                .or_insert_with(|| seg.to_string());
                        }
                    }
                }
            }

            if let Some(chunks) = meta.grounding_chunks {
                for (i, chunk) in chunks.into_iter().enumerate() {
                    if let Some(web) = chunk.web {
                        let url = web.uri.unwrap_or_default();
                        if !url.is_empty() {
                            let snippet = chunk_snippets
                                .remove(&i)
                                .unwrap_or_default();
                            results.push(SearchResult {
                                title: web.title.unwrap_or_default(),
                                url,
                                snippet,
                                published_at: None,
                            });
                        }
                    }
                }
            }
        }

        let total = Some(results.len() as u64);
        let with_snippets = results.iter().filter(|r| !r.snippet.is_empty()).count();
        tracing::info!(
            total = results.len(),
            with_snippets,
            "Google search results extracted"
        );
        Ok(SearchResults {
            results,
            total,
            query: req.query,
        })
    }
}
