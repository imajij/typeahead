use axum::{
    routing::get,
    Router,
};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

mod suggest;
mod cache;
mod db;
mod batch;
mod trending;
mod handlers;
mod models;
mod indexer;
mod pagerank;

#[derive(Clone)]
pub struct AppState {
    pub cache: cache::Cache,
    pub trending_cache: trending::TrendingCache,
    pub db_path: String,
    pub search_tx: tokio::sync::mpsc::Sender<String>,
    pub recent_logs: Arc<Mutex<Vec<String>>>,
}

impl AppState {
    /// Inserts a timestamped event log into the rolling buffer for dashboard telemetries.
    pub fn log(&self, msg: &str) {
        if let Ok(mut logs) = self.recent_logs.lock() {
            let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
            logs.insert(0, format!("[{}] {}", timestamp, msg));
            if logs.len() > 35 {
                logs.truncate(35);
            }
        }
    }
}

#[tokio::main]
async fn main() {
    println!("Initializing Google Search Simulator Engine (Backend Core)...");

    let db_path = "data/typeahead.db".to_string();

    // Create necessary folders
    std::fs::create_dir_all("data").expect("Failed to create data directory");
    std::fs::create_dir_all("static").expect("Failed to create static directory");

    // Initialize Database Connection and Seeding (Runs indexer and pagerank on boot)
    let conn = db::init_db(&db_path).expect("Failed to initialize database");
    
    // Fetch initial datasets to load into the caches
    let initial_queries = db::get_all_queries(&conn).expect("Failed to load queries from database");
    let initial_trending = db::get_trending_queries(&conn, 10, 60).expect("Failed to load trending queries");

    // Instantiate and populate Trie cache
    let cache = cache::Cache::new();
    cache.rebuild(initial_queries).await;

    // Instantiate and populate Trending cache
    let trending_cache = trending::TrendingCache::new();
    trending_cache.update(
        initial_trending
            .into_iter()
            .map(|(query, score)| models::TrendingQuery { query, score })
            .collect()
    ).await;

    // Build communications channel for the batch processing worker
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(2000);
    let recent_logs = Arc::new(Mutex::new(Vec::new()));

    let state = AppState {
        cache: cache.clone(),
        trending_cache: trending_cache.clone(),
        db_path: db_path.clone(),
        search_tx: tx,
        recent_logs: recent_logs.clone(),
    };

    state.log("SQLite relational tables connected.");
    state.log("PageRank linkages graph index active.");
    state.log("Inverted index postings computed.");
    state.log("Trie memory prefix suggestions active.");

    // Start background batching worker thread
    let db_path_clone = db_path.clone();
    let cache_clone = cache.clone();
    let trending_cache_clone = trending_cache.clone();
    let recent_logs_clone = recent_logs.clone();
    
    tokio::spawn(async move {
        batch::start_batch_worker(
            db_path_clone,
            rx,
            cache_clone,
            trending_cache_clone,
            recent_logs_clone,
        ).await;
    });

    // Configure Axum Web Server Router
    let app = Router::new()
        // Autocomplete/Suggestions API
        .route("/api/search", get(handlers::search_handler).post(handlers::record_search_handler))
        .route("/api/trending", get(handlers::trending_handler))
        // Search Engine Query ranked by TF-IDF + PageRank
        .route("/api/query", get(handlers::query_handler))
        // Crawler Ingest APIs
        .route("/api/documents", get(handlers::list_documents_handler).post(handlers::ingest_document_handler))
        // System stats
        .route("/api/stats", get(handlers::stats_handler))
        // Static file server
        .nest_service("/", ServeDir::new("static"))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Listening on http://{}", addr);
    
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}