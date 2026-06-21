use std::sync::Arc;
use tokio::sync::RwLock;
use crate::suggest::{SuggestEngine, Suggestion};

#[derive(Clone)]
pub struct Cache {
    engine: Arc<RwLock<SuggestEngine>>,
}

impl Cache {
    pub fn new() -> Self {
        Cache {
            engine: Arc::new(RwLock::new(SuggestEngine::new())),
        }
    }

    /// Builds a fresh engine from absolute (query, count) pairs and swaps it
    /// in. Only needed at boot; live traffic goes through `apply`.
    pub async fn rebuild(&self, queries: Vec<(String, u64)>) {
        let mut engine = SuggestEngine::new();
        for (query, count) in queries {
            engine.set_count(&query, count);
        }
        *self.engine.write().await = engine;
    }

    /// Applies incremental count deltas from the live query stream,
    /// updating top-K lists in place along each insert path.
    pub async fn apply(&self, deltas: &[(String, u64)]) {
        let mut engine = self.engine.write().await;
        for (query, delta) in deltas {
            engine.increment(query, *delta);
        }
    }

    /// Fetches the top suggestions (exact, mid-word, then fuzzy) for a prefix.
    pub async fn get_suggestions(&self, prefix: &str, limit: usize) -> Vec<Suggestion> {
        self.engine.read().await.suggest(prefix, limit)
    }

    /// Counts all nodes currently allocated in the trie index.
    pub async fn count_nodes(&self) -> usize {
        self.engine.read().await.node_count()
    }
}
