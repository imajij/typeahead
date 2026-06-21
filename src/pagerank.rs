use std::collections::HashMap;
use rusqlite::Connection;

/// Resolves page importance scores across the directed web link graph in SQLite.
/// Uses the power iteration PageRank algorithm with sink redistribution.
pub fn calculate_pagerank(conn: &Connection) -> Result<(), rusqlite::Error> {
    // 1. Fetch all documents and build mapping tables
    let mut stmt = conn.prepare("SELECT id, url FROM documents")?;
    let doc_rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut id_to_index = HashMap::new();
    let mut index_to_id = Vec::new();
    let mut url_to_index = HashMap::new();

    let mut doc_count = 0;
    for doc in doc_rows {
        let (id, url) = doc?;
        id_to_index.insert(id, doc_count);
        index_to_id.push(id);
        url_to_index.insert(url, doc_count);
        doc_count += 1;
    }

    if doc_count == 0 {
        return Ok(());
    }

    // 2. Fetch links to build adjacency lists
    let mut stmt = conn.prepare("SELECT from_url, to_url FROM links")?;
    let link_rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut out_degree = vec![0; doc_count];
    let mut in_links: Vec<Vec<usize>> = vec![Vec::new(); doc_count];

    for link in link_rows {
        let (from_url, to_url) = link?;
        if let (Some(&from_idx), Some(&to_idx)) = (url_to_index.get(&from_url), url_to_index.get(&to_url)) {
            // Dedup multiple linkages between same nodes
            if !in_links[to_idx].contains(&from_idx) {
                in_links[to_idx].push(from_idx);
                out_degree[from_idx] += 1;
            }
        }
    }

    // 3. PageRank Power Iteration
    let damping_factor = 0.85;
    let mut pr = vec![1.0 / (doc_count as f64); doc_count];
    let iterations = 20;

    for _ in 0..iterations {
        let mut next_pr = vec![0.0; doc_count];
        
        // Sum of rank from pages with no outgoing edges (sink nodes)
        let mut sink_sum = 0.0;
        for i in 0..doc_count {
            if out_degree[i] == 0 {
                sink_sum += pr[i];
            }
        }

        // Standard damping + sink redistribution
        let base_value = (1.0 - damping_factor) / (doc_count as f64) + damping_factor * (sink_sum / (doc_count as f64));

        for i in 0..doc_count {
            let mut sum = 0.0;
            for &incoming_idx in &in_links[i] {
                sum += pr[incoming_idx] / (out_degree[incoming_idx] as f64);
            }
            next_pr[i] = base_value + damping_factor * sum;
        }
        pr = next_pr;
    }

    // Normalize PageRank scores so they sum to 1.0 (optional, but good for display)
    let sum: f64 = pr.iter().sum();
    if sum > 0.0 {
        for val in pr.iter_mut() {
            *val /= sum;
        }
    }

    // 4. Update documents' PageRank values inside SQLite
    let mut stmt_update = conn.prepare("UPDATE documents SET pagerank = ?1 WHERE id = ?2")?;
    for i in 0..doc_count {
        stmt_update.execute(rusqlite::params![pr[i], index_to_id[i]])?;
    }

    Ok(())
}
