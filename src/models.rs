use serde::{Deserialize, Serialize};

// Autocomplete suggestions models
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct SearchSuggestion {
    pub query: String,
    pub count: u64,
    /// True when the suggestion came from typo-tolerant matching rather
    /// than an exact prefix/word match.
    pub fuzzy: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SearchRequest {
    pub query: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SearchResponse {
    pub suggestions: Vec<SearchSuggestion>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct TrendingQuery {
    pub query: String,
    pub score: u32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TrendingResponse {
    pub trending: Vec<TrendingQuery>,
}

// Search Index Match Results models
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct SearchMatch {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
    pub tf_idf_score: f64,
    pub pagerank_score: f64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct QueryResultResponse {
    pub results: Vec<SearchMatch>,
    /// Server-side ranking time, for the "About N results (0.03 seconds)" line.
    pub elapsed_ms: f64,
}

// Ingest Crawler models
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Document {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub body: String,
    pub pagerank: f64,
}

#[derive(Deserialize, Debug)]
pub struct DocumentIngestRequest {
    pub url: String,
    pub title: String,
    pub body: String,
    pub links: Vec<String>,
}

// System stats
#[derive(Serialize, Deserialize, Debug)]
pub struct SystemStats {
    pub total_queries: usize,
    pub total_indexed_documents: usize,
    pub active_trie_nodes: usize,
    pub queue_size: usize,
    pub database_size_bytes: u64,
    pub recent_logs: Vec<String>,
}
