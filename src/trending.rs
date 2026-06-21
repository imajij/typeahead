use std::sync::Arc;
use tokio::sync::RwLock;
use crate::models::TrendingQuery;

#[derive(Clone)]
pub struct TrendingCache {
    queries: Arc<RwLock<Vec<TrendingQuery>>>,
}

impl TrendingCache {
    pub fn new() -> Self {
        TrendingCache {
            queries: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Safely updates the trending searches in the cache.
    pub async fn update(&self, new_trending: Vec<TrendingQuery>) {
        let mut w = self.queries.write().await;
        *w = new_trending;
    }

    /// Retrieves the current trending searches.
    pub async fn get_trending(&self) -> Vec<TrendingQuery> {
        let r = self.queries.read().await;
        r.clone()
    }
}
