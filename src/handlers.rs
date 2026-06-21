use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use crate::AppState;
use crate::models::{
    SearchRequest, SearchResponse, SearchSuggestion, SystemStats, TrendingResponse,
    QueryResultResponse, Document, DocumentIngestRequest
};

#[derive(Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub limit: Option<usize>,
}

/// Handler for GET /api/search?q=prefix
/// Fetches autocomplete suggestions: exact prefix and mid-query word matches
/// first, typo-tolerant (fuzzy) matches filling the remainder.
pub async fn search_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Json<SearchResponse> {
    let prefix = crate::suggest::normalize_query(&params.q.unwrap_or_default());
    if prefix.is_empty() {
        return Json(SearchResponse { suggestions: Vec::new() });
    }

    let limit = params.limit.unwrap_or(8).clamp(1, 10);
    let suggestions = state
        .cache
        .get_suggestions(&prefix, limit)
        .await
        .into_iter()
        .map(|s| SearchSuggestion {
            query: s.text,
            count: s.count,
            fuzzy: s.fuzzy,
        })
        .collect();

    Json(SearchResponse { suggestions })
}

/// Handler for POST /api/search
/// Enqueues autocomplete search query logging.
pub async fn record_search_handler(
    State(state): State<AppState>,
    Json(payload): Json<SearchRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let query = crate::suggest::normalize_query(&payload.query);
    if query.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Query cannot be empty" })),
        );
    }

    match state.search_tx.send(query.clone()).await {
        Ok(_) => {
            state.log(&format!("Enqueued search query: \"{}\"", query));
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "status": "queued" })),
            )
        }
        Err(_) => {
            state.log("Failed to enqueue search query: write channel full.");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Internal queue full" })),
            )
        }
    }
}

/// Handler for GET /api/trending
/// Fetches current trending searches.
pub async fn trending_handler(
    State(state): State<AppState>,
) -> Json<TrendingResponse> {
    let trending = state.trending_cache.get_trending().await;
    Json(TrendingResponse { trending })
}

/// Handler for GET /api/query?q=search_text
/// Queries the inverted index and returns search matches ranked by TF-IDF + PageRank.
pub async fn query_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> (StatusCode, Json<QueryResultResponse>) {
    let query_text = params.q.unwrap_or_default().trim().to_string();
    if query_text.is_empty() {
        return (StatusCode::OK, Json(QueryResultResponse { results: Vec::new(), elapsed_ms: 0.0 }));
    }

    state.log(&format!("Executing search query: \"{}\"", query_text));

    let started = std::time::Instant::now();
    let db_path = state.db_path.clone();
    let query_clone = query_text.clone();
    let search_result = tokio::task::spawn_blocking(move || {
        let conn = rusqlite::Connection::open(&db_path)?;
        crate::indexer::search_and_rank(&conn, &query_clone)
    }).await;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

    match search_result {
        Ok(Ok(results)) => {
            state.log(&format!("Query \"{}\" returned {} matched documents.", query_text, results.len()));
            (StatusCode::OK, Json(QueryResultResponse { results, elapsed_ms }))
        }
        Ok(Err(e)) => {
            state.log(&format!("Error executing query \"{}\": {}", query_text, e));
            (StatusCode::INTERNAL_SERVER_ERROR, Json(QueryResultResponse { results: Vec::new(), elapsed_ms }))
        }
        Err(_) => {
            state.log("Task join error during query ranking.");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(QueryResultResponse { results: Vec::new(), elapsed_ms }))
        }
    }
}

/// Handler for POST /api/documents
/// Ingests a new crawled document, runs indexing, and recalculates PageRank.
pub async fn ingest_document_handler(
    State(state): State<AppState>,
    Json(payload): Json<DocumentIngestRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let url = payload.url.trim().to_string();
    let title = payload.title.trim().to_string();
    let body = payload.body.trim().to_string();

    if url.is_empty() || title.is_empty() || body.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "URL, Title, and Body cannot be empty" })),
        );
    }

    state.log(&format!("Crawler ingesting document: \"{}\"", title));

    let url_clone = url.clone();
    let title_clone = title.clone();
    let db_path = state.db_path.clone();
    let ingest_result = tokio::task::spawn_blocking(move || {
        let mut conn = rusqlite::Connection::open(&db_path)?;
        let tx = conn.transaction()?;
        
        let doc_id: i64;
        {
            // Insert or update document
            tx.execute(
                "INSERT INTO documents (url, title, body) 
                 VALUES (?1, ?2, ?3) 
                 ON CONFLICT(url) DO UPDATE SET title = ?2, body = ?3",
                rusqlite::params![url_clone, title_clone, body]
            )?;
            
            doc_id = tx.query_row(
                "SELECT id FROM documents WHERE url = ?1",
                rusqlite::params![url_clone],
                |row| row.get(0)
            )?;

            // Delete old linkages for this page
            tx.execute("DELETE FROM links WHERE from_url = ?1", rusqlite::params![url_clone])?;

            // Insert new linkages
            let mut stmt_link = tx.prepare(
                "INSERT OR IGNORE INTO links (from_url, to_url) VALUES (?1, ?2)"
            )?;
            for link in &payload.links {
                let to_url = link.trim().to_string();
                if !to_url.is_empty() && to_url != url_clone {
                    stmt_link.execute(rusqlite::params![url_clone, to_url])?;
                }
            }
        }
        
        tx.commit()?;

        // Perform inverted index indexing
        let index_conn = rusqlite::Connection::open(&db_path)?;
        crate::indexer::index_document(&index_conn, doc_id, &body)?;

        // Re-calculate PageRank graph scores
        crate::pagerank::calculate_pagerank(&index_conn)?;

        Ok::<_, rusqlite::Error>(doc_id)
    }).await;

    match ingest_result {
        Ok(Ok(_doc_id)) => {
            state.log(&format!("Successfully indexed document \"{}\" and recalculated PageRank.", title));
            (StatusCode::CREATED, Json(serde_json::json!({ "status": "indexed", "url": url })))
        }
        Ok(Err(e)) => {
            state.log(&format!("Error ingesting document: {}", e));
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() })))
        }
        Err(_) => {
            state.log("Task join error during doc ingestion.");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "Internal join error" })))
        }
    }
}

/// Handler for GET /api/documents
/// Returns all indexed documents sorted by PageRank authority scores.
pub async fn list_documents_handler(
    State(state): State<AppState>,
) -> (StatusCode, Json<Vec<Document>>) {
    let db_path = state.db_path.clone();
    let query_result = tokio::task::spawn_blocking(move || {
        let conn = rusqlite::Connection::open(&db_path)?;
        let mut stmt = conn.prepare(
            "SELECT id, url, title, body, pagerank FROM documents ORDER BY pagerank DESC"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Document {
                id: row.get(0)?,
                url: row.get(1)?,
                title: row.get(2)?,
                body: row.get(3)?,
                pagerank: row.get(4)?,
            })
        })?;
        
        let mut docs = Vec::new();
        for r in rows {
            docs.push(r?);
        }
        Ok::<_, rusqlite::Error>(docs)
    }).await;

    match query_result {
        Ok(Ok(docs)) => (StatusCode::OK, Json(docs)),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(Vec::new())),
    }
}

/// Handler for GET /api/stats
/// Compiles telemetry details of the search database.
pub async fn stats_handler(
    State(state): State<AppState>,
) -> Json<SystemStats> {
    let queue_size = state.search_tx.max_capacity() - state.search_tx.capacity();
    let active_trie_nodes = state.cache.count_nodes().await;

    // Database size on disk
    let db_size = match std::fs::metadata(&state.db_path) {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    };

    // Database row count statistics
    let db_path = state.db_path.clone();
    let stats_result = tokio::task::spawn_blocking(move || {
        if let Ok(conn) = rusqlite::Connection::open(&db_path) {
            let total_queries: i64 = conn.query_row("SELECT COUNT(*) FROM queries", [], |row| row.get(0)).unwrap_or(0);
            let total_docs: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0)).unwrap_or(0);
            (total_queries as usize, total_docs as usize)
        } else {
            (0, 0)
        }
    }).await;

    let (total_queries, total_indexed_documents) = stats_result.unwrap_or((0, 0));

    let recent_logs = if let Ok(logs) = state.recent_logs.lock() {
        logs.clone()
    } else {
        Vec::new()
    };

    Json(SystemStats {
        total_queries,
        total_indexed_documents,
        active_trie_nodes,
        queue_size,
        database_size_bytes: db_size,
        recent_logs,
    })
}
