//! Search capability.

use async_trait::async_trait;

pub struct SearchRequest {
    pub query: String,
    pub limit: Option<usize>,
    pub search_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SearchResults {
    pub results: Vec<SearchResult>,
    pub total: Option<u64>,
    pub query: String,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub published_at: Option<String>,
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn search(&self, req: SearchRequest) -> anyhow::Result<SearchResults>;
}
