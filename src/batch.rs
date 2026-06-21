use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;
use tokio::time::MissedTickBehavior;
use rusqlite::Connection;
use crate::cache::Cache;
use crate::trending::TrendingCache;
use crate::models::TrendingQuery;

const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const MAX_BUFFER_SIZE: u64 = 50;
const TRENDING_LIMIT: usize = 10;
const TRENDING_WINDOW_MINUTES: u32 = 60;

/// Background worker that processes the incoming search query stream.
/// Aggregates deltas in memory and periodically batch-writes SQLite, then
/// applies the same deltas to the suggestion trie in place — no rebuild.
pub async fn start_batch_worker(
    db_path: String,
    mut rx: Receiver<String>,
    cache: Cache,
    trending_cache: TrendingCache,
    recent_logs: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    let log_fn = move |msg: &str| {
        if let Ok(mut logs) = recent_logs.lock() {
            let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
            logs.insert(0, format!("[{}] {}", timestamp, msg));
            if logs.len() > 30 {
                logs.truncate(30);
            }
        }
    };

    log_fn("Batch worker started successfully.");

    let mut buffer: HashMap<String, u64> = HashMap::new();
    let mut flush_tick = tokio::time::interval(FLUSH_INTERVAL);
    flush_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let flush_now;
        let mut shutting_down = false;

        tokio::select! {
            maybe_query = rx.recv() => {
                match maybe_query {
                    Some(query) => {
                        *buffer.entry(query).or_insert(0) += 1;
                        flush_now = buffer.values().sum::<u64>() >= MAX_BUFFER_SIZE;
                    }
                    None => {
                        log_fn("Search channel closed. Shutting down batch worker.");
                        flush_now = !buffer.is_empty();
                        shutting_down = true;
                    }
                }
            }
            _ = flush_tick.tick() => {
                flush_now = !buffer.is_empty();
            }
        }

        if flush_now {
            // Buffer is kept on failure so the deltas retry on the next tick.
            if flush_batch(&db_path, &buffer, &cache, &trending_cache, &log_fn).await {
                buffer.clear();
            }
        }

        if shutting_down {
            break;
        }
    }
}

/// Writes a batch of query-count deltas to SQLite, refreshes the trending
/// cache from the log table, and applies the deltas to the in-memory trie.
async fn flush_batch(
    db_path: &str,
    buffer: &HashMap<String, u64>,
    cache: &Cache,
    trending_cache: &TrendingCache,
    log_fn: &impl Fn(&str),
) -> bool {
    log_fn(&format!("Flushing batch of {} unique queries...", buffer.len()));

    let db_path = db_path.to_string();
    let deltas: Vec<(String, u64)> = buffer.iter().map(|(q, c)| (q.clone(), *c)).collect();
    let deltas_for_db = deltas.clone();

    let db_result = tokio::task::spawn_blocking(move || {
        let mut conn = Connection::open(&db_path)?;
        let tx = conn.transaction()?;
        {
            let mut stmt_queries = tx.prepare(
                "INSERT INTO queries (query, count, updated_at)
                 VALUES (?1, ?2, CURRENT_TIMESTAMP)
                 ON CONFLICT(query) DO UPDATE SET
                    count = count + ?2,
                    updated_at = CURRENT_TIMESTAMP",
            )?;
            let mut stmt_logs = tx.prepare("INSERT INTO query_logs (query) VALUES (?1)")?;

            for (query, count) in &deltas_for_db {
                stmt_queries.execute(rusqlite::params![query, count])?;
                for _ in 0..*count {
                    stmt_logs.execute(rusqlite::params![query])?;
                }
            }
        }
        tx.commit()?;

        crate::db::get_trending_queries(&conn, TRENDING_LIMIT, TRENDING_WINDOW_MINUTES)
    })
    .await;

    match db_result {
        Ok(Ok(trending)) => {
            // Incremental trie update: O(query length) per delta, no rebuild.
            cache.apply(&deltas).await;
            log_fn(&format!(
                "Batch committed. {} deltas applied to suggestion trie in place.",
                deltas.len()
            ));

            let trending_queries = trending
                .into_iter()
                .map(|(query, score)| TrendingQuery { query, score })
                .collect();
            trending_cache.update(trending_queries).await;
            true
        }
        Ok(Err(e)) => {
            log_fn(&format!("Error writing to database: {}", e));
            false
        }
        Err(e) => {
            log_fn(&format!("Task join error during DB write: {}", e));
            false
        }
    }
}
