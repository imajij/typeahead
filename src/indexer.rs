use rusqlite::{params, Connection, Result};
use crate::models::SearchMatch;
use std::collections::HashMap;

/// Simple word tokenizer that converts text to lowercase,
/// filters out punctuation, and returns words of length >= 2.
pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|w| w.len() >= 2)
        .map(|s| s.to_string())
        .collect()
}

/// Tokenizes a document body, calculates term frequencies,
/// and updates the database's inverted index postings table.
pub fn index_document(conn: &Connection, doc_id: i64, body: &str) -> Result<()> {
    let tokens = tokenize(body);
    let mut term_frequencies = HashMap::new();
    for token in tokens {
        *term_frequencies.entry(token).or_insert(0) += 1;
    }

    // Delete existing index records for this document
    conn.execute("DELETE FROM inverted_index WHERE doc_id = ?1", params![doc_id])?;

    // Bulk insert new postings into inverted index
    let mut stmt = conn.prepare(
        "INSERT INTO inverted_index (term, doc_id, term_frequency) VALUES (?1, ?2, ?3)"
    )?;

    for (term, freq) in term_frequencies {
        stmt.execute(params![term, doc_id, freq])?;
    }

    Ok(())
}

/// Tokenizes a search query, retrieves posting lists from the database,
/// calculates TF-IDF scores, and combines them with PageRank values.
pub fn search_and_rank(conn: &Connection, query: &str) -> Result<Vec<SearchMatch>> {
    let query_terms = tokenize(query);
    if query_terms.is_empty() {
        return Ok(Vec::new());
    }

    // 1. Fetch total document count N
    let total_docs: i64 = conn.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))?;
    if total_docs == 0 {
        return Ok(Vec::new());
    }

    // 2. Fetch postings for query terms
    let mut term_placeholders = Vec::new();
    for i in 0..query_terms.len() {
        term_placeholders.push(format!("?{}", i + 1));
    }
    
    let sql_postings = format!(
        "SELECT term, doc_id, term_frequency FROM inverted_index WHERE term IN ({})",
        term_placeholders.join(",")
    );
    
    let mut stmt = conn.prepare(&sql_postings)?;
    let params_iter = rusqlite::params_from_iter(query_terms.iter());
    
    let rows = stmt.query_map(params_iter, |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
    })?;

    // Group postings by term
    let mut term_postings = HashMap::new();
    for row in rows {
        let (term, doc_id, tf) = row?;
        term_postings.entry(term.clone()).or_insert_with(Vec::new).push((doc_id, tf));
    }

    // Calculate IDF for each query term: ln(1 + (N - DF + 0.5) / (DF + 0.5))
    let mut idfs = HashMap::new();
    for term in &query_terms {
        let df = term_postings.get(term).map(|p| p.len()).unwrap_or(0) as f64;
        let idf = ((total_docs as f64 - df + 0.5) / (df + 0.5)).max(0.0001).ln();
        idfs.insert(term.clone(), idf);
    }

    // 3. Aggregate TF-IDF scores for matching documents
    let mut doc_scores = HashMap::new();
    for (term, postings) in &term_postings {
        let idf = idfs.get(term).copied().unwrap_or(0.0);
        for &(doc_id, tf) in postings {
            let score = (tf as f64) * idf;
            *doc_scores.entry(doc_id).or_insert(0.0) += score;
        }
    }

    // 4. Retrieve document metadata and merge with PageRank
    let mut results = Vec::new();
    let mut stmt_doc = conn.prepare("SELECT url, title, body, pagerank FROM documents WHERE id = ?1")?;

    for (doc_id, tf_idf) in doc_scores {
        let (url, title, body, pr) = stmt_doc.query_row(params![doc_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })?;

        // Combined Score = TF-IDF * (1.0 + 10.0 * PageRank)
        let score = tf_idf * (1.0 + 10.0 * pr);
        
        let snippet = create_snippet(&body, &query_terms);

        results.push(SearchMatch {
            url,
            title,
            snippet,
            score,
            tf_idf_score: tf_idf,
            pagerank_score: pr,
        });
    }

    // 5. Sort by combined score descending
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    Ok(results)
}

/// Generates a brief text snippet centered around the first occurrence of any query term.
fn create_snippet(body: &str, query_terms: &[String]) -> String {
    let lower_body = body.to_lowercase();
    let mut match_idx = None;

    for term in query_terms {
        if let Some(idx) = lower_body.find(term) {
            match_idx = Some(idx);
            break;
        }
    }

    let start = match_idx.unwrap_or(0);
    let snippet_start = if start > 45 { start - 45 } else { 0 };
    let snippet_end = (snippet_start + 150).min(body.len());
    
    let mut snippet = body[snippet_start..snippet_end].to_string();
    if snippet_start > 0 {
        snippet.insert_str(0, "... ");
    }
    if snippet_end < body.len() {
        snippet.push_str(" ...");
    }
    
    snippet
}
