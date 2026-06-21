use std::collections::{HashMap, HashSet};

/// Number of precomputed suggestions stored per trie node.
const TOP_K: usize = 10;

/// A single autocomplete candidate returned by the engine.
#[derive(Clone, Debug)]
pub struct Suggestion {
    pub text: String,
    pub count: u64,
    pub fuzzy: bool,
}

#[derive(Clone, Copy, Debug)]
struct ScoredRef {
    id: u32,
    count: u64,
}

#[derive(Debug, Default)]
struct TrieNode {
    children: HashMap<char, TrieNode>,
    /// Top queries (by count) among everything indexed under this node.
    /// Kept up to date incrementally on every insert/increment, so lookups
    /// are O(prefix length) with zero post-processing.
    top: Vec<ScoredRef>,
}

#[derive(Debug)]
struct Entry {
    text: String,
    count: u64,
}

/// Google-style suggestion engine.
///
/// Every query is indexed in the trie under each of its word-boundary
/// suffixes ("react hooks guide" is reachable from "react…", "hooks…" and
/// "guide…"), all pointing at one canonical entry. Each node carries a
/// precomputed top-K list that is updated in place along the insert path,
/// so live traffic never requires a full rebuild. Lookups fall back to a
/// bounded Damerau-Levenshtein walk over the trie for typo tolerance.
#[derive(Default)]
pub struct SuggestEngine {
    root: TrieNode,
    entries: Vec<Entry>,
    by_text: HashMap<String, u32>,
}

/// Canonical form used for both indexing and lookups: lowercased,
/// whitespace-collapsed, capped at 100 chars.
pub fn normalize_query(raw: &str) -> String {
    let collapsed = raw
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.chars().take(100).collect()
}

impl SuggestEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the absolute popularity count for a query (initial load from DB).
    pub fn set_count(&mut self, text: &str, count: u64) {
        let id = self.intern(text);
        self.entries[id as usize].count = count;
        self.reindex(id);
    }

    /// Adds `delta` to a query's count, inserting it if new (live updates).
    pub fn increment(&mut self, text: &str, delta: u64) {
        let id = self.intern(text);
        self.entries[id as usize].count += delta;
        self.reindex(id);
    }

    /// Returns up to `limit` suggestions for the prefix: exact prefix and
    /// mid-query word matches first, typo-tolerant matches to fill the rest.
    pub fn suggest(&self, prefix: &str, limit: usize) -> Vec<Suggestion> {
        if prefix.is_empty() || limit == 0 {
            return Vec::new();
        }

        let mut seen = HashSet::new();
        let mut out = Vec::new();

        if let Some(node) = self.walk(prefix) {
            for r in &node.top {
                if seen.insert(r.id) {
                    out.push(Suggestion {
                        text: self.entries[r.id as usize].text.clone(),
                        count: r.count,
                        fuzzy: false,
                    });
                }
            }
        }

        if out.len() < limit {
            let max_edits = match prefix.chars().count() {
                0..=2 => 0,
                3..=5 => 1,
                _ => 2,
            };
            if max_edits > 0 {
                let mut found: HashMap<u32, (u64, usize)> = HashMap::new();
                self.fuzzy_collect(prefix, max_edits, &mut found);

                let mut candidates: Vec<(u32, u64, usize)> = found
                    .into_iter()
                    .filter(|(id, _)| !seen.contains(id))
                    .map(|(id, (count, dist))| (id, count, dist))
                    .collect();
                // Closest correction first, popularity breaks ties.
                candidates.sort_by(|a, b| a.2.cmp(&b.2).then(b.1.cmp(&a.1)).then(a.0.cmp(&b.0)));

                for (id, count, _) in candidates {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(Suggestion {
                        text: self.entries[id as usize].text.clone(),
                        count,
                        fuzzy: true,
                    });
                }
            }
        }

        out.truncate(limit);
        out
    }

    /// Total trie nodes currently allocated (for the stats endpoint).
    pub fn node_count(&self) -> usize {
        fn rec(node: &TrieNode) -> usize {
            1 + node.children.values().map(rec).sum::<usize>()
        }
        rec(&self.root)
    }

    fn intern(&mut self, text: &str) -> u32 {
        if let Some(&id) = self.by_text.get(text) {
            return id;
        }
        let id = self.entries.len() as u32;
        self.entries.push(Entry {
            text: text.to_string(),
            count: 0,
        });
        self.by_text.insert(text.to_string(), id);
        id
    }

    /// Re-walks every index key of the entry, refreshing top-K lists in place.
    /// Updating by canonical id keeps the lists duplicate-free even when
    /// multiple suffix keys share prefix nodes.
    fn reindex(&mut self, id: u32) {
        let text = self.entries[id as usize].text.clone();
        let count = self.entries[id as usize].count;

        for key in index_keys(&text) {
            let mut node = &mut self.root;
            update_top(&mut node.top, id, count);
            for ch in key.chars() {
                node = node.children.entry(ch).or_default();
                update_top(&mut node.top, id, count);
            }
        }
    }

    fn walk(&self, prefix: &str) -> Option<&TrieNode> {
        let mut node = &self.root;
        for ch in prefix.chars() {
            node = node.children.get(&ch)?;
        }
        Some(node)
    }

    /// Bounded Damerau-Levenshtein walk: visits every trie path within
    /// `max_edits` of the prefix and collects the candidates indexed below it.
    fn fuzzy_collect(&self, prefix: &str, max_edits: usize, found: &mut HashMap<u32, (u64, usize)>) {
        let pattern: Vec<char> = prefix.chars().collect();
        let first_row: Vec<usize> = (0..=pattern.len()).collect();
        for (&ch, child) in &self.root.children {
            Self::fuzzy_dfs(child, ch, None, &pattern, &first_row, None, max_edits, found);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn fuzzy_dfs(
        node: &TrieNode,
        ch: char,
        prev_ch: Option<char>,
        pattern: &[char],
        prev_row: &[usize],
        prev_prev_row: Option<&[usize]>,
        max_edits: usize,
        found: &mut HashMap<u32, (u64, usize)>,
    ) {
        let mut row = Vec::with_capacity(pattern.len() + 1);
        row.push(prev_row[0] + 1);
        for i in 1..=pattern.len() {
            let cost = if pattern[i - 1] == ch { 0 } else { 1 };
            let mut d = (row[i - 1] + 1)
                .min(prev_row[i] + 1)
                .min(prev_row[i - 1] + cost);
            // Adjacent transposition ("raect" -> "react") counts as one edit.
            if i >= 2 {
                if let (Some(pc), Some(pp)) = (prev_ch, prev_prev_row) {
                    if pattern[i - 1] == pc && pattern[i - 2] == ch {
                        d = d.min(pp[i - 2] + 1);
                    }
                }
            }
            row.push(d);
        }

        let dist = row[pattern.len()];
        if dist <= max_edits {
            for r in &node.top {
                let e = found.entry(r.id).or_insert((r.count, dist));
                if dist < e.1 || (dist == e.1 && r.count > e.0) {
                    *e = (r.count, dist);
                }
            }
        }

        // Descendants can only get closer while some band of the row is in
        // budget; row minimum grows once the path overshoots the pattern.
        if row.iter().min().copied().unwrap_or(usize::MAX) <= max_edits {
            for (&c, child) in &node.children {
                Self::fuzzy_dfs(child, c, Some(ch), pattern, &row, Some(prev_row), max_edits, found);
            }
        }
    }
}

/// The full query plus each word-boundary suffix, so any word in the query
/// can start a match.
fn index_keys(text: &str) -> Vec<&str> {
    let mut keys = vec![text];
    for (i, ch) in text.char_indices() {
        if ch == ' ' {
            let suffix = &text[i + 1..];
            if !suffix.is_empty() && !keys.contains(&suffix) {
                keys.push(suffix);
            }
        }
    }
    keys
}

fn update_top(top: &mut Vec<ScoredRef>, id: u32, count: u64) {
    match top.iter_mut().find(|r| r.id == id) {
        Some(r) => r.count = count,
        None => top.push(ScoredRef { id, count }),
    }
    top.sort_by(|a, b| b.count.cmp(&a.count).then(a.id.cmp(&b.id)));
    top.truncate(TOP_K);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> SuggestEngine {
        let mut e = SuggestEngine::new();
        e.set_count("rust programming", 1500);
        e.set_count("react js tutorial", 1200);
        e.set_count("react hooks guide", 950);
        e.set_count("javascript async await", 890);
        e
    }

    #[test]
    fn prefix_matches_ranked_by_count() {
        let e = engine();
        let s = e.suggest("re", 10);
        let texts: Vec<&str> = s.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, vec!["react js tutorial", "react hooks guide"]);
        assert!(s.iter().all(|x| !x.fuzzy));
    }

    #[test]
    fn mid_query_word_matches() {
        let e = engine();
        let s = e.suggest("hooks", 5);
        assert!(s.iter().any(|x| x.text == "react hooks guide" && !x.fuzzy));
        let s = e.suggest("async", 5);
        assert!(s.iter().any(|x| x.text == "javascript async await"));
    }

    #[test]
    fn fuzzy_corrects_typos() {
        let e = engine();
        // Substitution: "reat" -> "reac…"
        let s = e.suggest("reat", 5);
        assert!(s.iter().any(|x| x.text == "react js tutorial" && x.fuzzy));
        // Transposition: "raect" -> "react…"
        let s = e.suggest("raect", 5);
        assert!(s.iter().any(|x| x.text == "react js tutorial" && x.fuzzy));
    }

    #[test]
    fn short_prefixes_skip_fuzzy() {
        let e = engine();
        let s = e.suggest("zz", 5);
        assert!(s.is_empty());
    }

    #[test]
    fn incremental_updates_rerank_in_place() {
        let mut e = engine();
        assert_eq!(e.suggest("react", 1)[0].text, "react js tutorial");
        e.increment("react hooks guide", 1000);
        assert_eq!(e.suggest("react", 1)[0].text, "react hooks guide");
        // Brand-new query becomes suggestible immediately.
        e.increment("react native vs flutter", 3);
        assert!(e.suggest("react na", 5).iter().any(|x| x.text == "react native vs flutter"));
    }

    #[test]
    fn duplicate_words_do_not_duplicate_suggestions() {
        let mut e = SuggestEngine::new();
        e.set_count("go go go", 10);
        let s = e.suggest("go", 10);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn normalization_collapses_whitespace() {
        assert_eq!(normalize_query("  ReAcT   Hooks \t Guide "), "react hooks guide");
    }
}
