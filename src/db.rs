use rusqlite::{params, Connection, Result};

pub fn init_db(db_path: &str) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    
    // Enable Write-Ahead Logging (WAL) for better concurrent performance
    conn.pragma_update(None, "journal_mode", &"WAL")?;
    
    // 1. Core query tracking tables (for autocomplete typeahead)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS queries (
            query TEXT PRIMARY KEY,
            count INTEGER NOT NULL DEFAULT 0,
            updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS query_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            query TEXT NOT NULL,
            timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_query_logs_timestamp ON query_logs(timestamp)",
        [],
    )?;

    // 2. Search Engine document index & link tables
    conn.execute(
        "CREATE TABLE IF NOT EXISTS documents (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            url TEXT UNIQUE NOT NULL,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            pagerank REAL DEFAULT 0.0
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS links (
            from_url TEXT NOT NULL,
            to_url TEXT NOT NULL,
            PRIMARY KEY (from_url, to_url)
        )",
        [],
    )?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS inverted_index (
            term TEXT NOT NULL,
            doc_id INTEGER NOT NULL,
            term_frequency INTEGER NOT NULL,
            PRIMARY KEY (term, doc_id),
            FOREIGN KEY(doc_id) REFERENCES documents(id) ON DELETE CASCADE
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_inverted_index_term ON inverted_index(term)",
        [],
    )?;

    // Check if we need to seed the database
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM documents",
        [],
        |row| row.get(0),
    )?;

    if count == 0 {
        seed_db(&conn)?;
    }

    Ok(conn)
}

fn seed_db(conn: &Connection) -> Result<()> {
    // 1. Seed autocomplete queries — a realistic mix of phrasings so the
    // typeahead demos prefix, mid-word, and typo matching well.
    let seed_queries = vec![
        ("rust programming", 2400),
        ("rust programming language", 1900),
        ("rust tutorial for beginners", 1100),
        ("rust async await", 880),
        ("rust concurrency models", 850),
        ("rust vs go performance", 720),
        ("rust ownership and borrowing", 640),
        ("rust cargo commands", 310),
        ("rust compiler errors explained", 280),
        ("tokio async runtime", 780),
        ("tokio channels tutorial", 350),
        ("react js tutorial", 2100),
        ("react hooks guide", 1500),
        ("react server components", 940),
        ("react useeffect cleanup", 690),
        ("react vs vue", 560),
        ("javascript async await", 1300),
        ("javascript event loop", 760),
        ("javascript promises explained", 540),
        ("sqlite database performance", 620),
        ("sqlite vs postgres", 530),
        ("sqlite wal mode", 410),
        ("trie data structure", 470),
        ("trie prefix tree autocomplete", 430),
        ("typeahead autocomplete design", 450),
        ("how does google search work", 980),
        ("how google pagerank works", 580),
        ("how to build a search engine", 720),
        ("inverted index explained", 390),
        ("tf idf ranking explained", 320),
        ("what is concurrency", 510),
        ("what is a virtual dom", 380),
        ("web crawler tutorial", 360),
        ("data structures and algorithms", 1200),
        ("system design interview questions", 1050),
        ("machine learning basics", 980),
        ("best programming language 2026", 800),
        ("weather today", 3100),
        ("news headlines today", 1700),
        ("translate english to spanish", 1450),
    ];

    let mut stmt = conn.prepare("INSERT OR REPLACE INTO queries (query, count) VALUES (?1, ?2)")?;
    for (query, cnt) in seed_queries {
        stmt.execute(params![query, cnt])?;
        
        let num_logs = (cnt / 25).max(1).min(20);
        let mut log_stmt = conn.prepare(
            "INSERT INTO query_logs (query, timestamp) VALUES (?1, datetime('now', ?2))"
        )?;
        
        for i in 0..num_logs {
            let offset_str = format!("-{} minutes", i * 3);
            log_stmt.execute(params![query, offset_str])?;
        }
    }

    // 2. Seed documents representing a mock tech web graph
    let seed_docs = vec![
        (
            "https://rust-lang.org/intro",
            "Introduction to Rust Programming",
            "Rust is a systems programming language focusing on safety, speed, and concurrency. It enforces memory safety without using a garbage collector. The compiler uses lifetime checks and ownership borrowing rules to prevent common errors like null pointer dereferences and data races."
        ),
        (
            "https://rust-lang.org/concurrency",
            "Rust Concurrency: Threads and Channels",
            "Concurrency in Rust is safe and performant. Rust threads provide parallel execution, while channels (mpsc) allow threads to pass messages safely. By utilizing ownership traits like Send and Sync, the Rust compiler guarantees that code is free of data races."
        ),
        (
            "https://tokio.rs/async",
            "Tokio Async Runtime: Multithreading in Rust",
            "Tokio is an event-driven, non-blocking async runtime for the Rust programming language. It provides thread pools, timers, and I/O abstractions. Tokio helps write high-throughput concurrent networking services and handles task scheduling efficiently."
        ),
        (
            "https://react.dev/start",
            "React Web Apps: Getting Started",
            "React is a popular JavaScript library for building component-based User Interfaces (UI). React applications render efficiently using a virtual DOM. It is widely used in modern web development alongside Javascript, HTML, and CSS."
        ),
        (
            "https://react.dev/hooks",
            "React Hooks: Managing Component State",
            "React Hooks like useState and useEffect allow developers to manage state and side effects inside functional components. Hooks promote reusable component logic and simplify complex state transitions in javascript frontend applications."
        ),
        (
            "https://js-guide.com/async",
            "Javascript Concurrency: Async Await and Promises",
            "Javascript is single-threaded but achieves concurrency using an event loop. Promises and async/await syntax allow non-blocking operations. Learning javascript async patterns is essential for react web development and API integration."
        ),
        (
            "https://sqlite.org/about",
            "SQLite: Lightweight Serverless Database",
            "SQLite is a serverless, self-contained relational database engine. It stores data in a single file on disk and is configured with transactional SQL query capabilities. SQLite is highly performant and widely used in mobile apps and lightweight backends."
        ),
        (
            "https://sqlite.org/wal-mode",
            "SQLite Performance: WAL Journal Mode",
            "Write-Ahead Logging (WAL) is a high-performance journal mode in SQLite. WAL allows multiple concurrent readers to fetch database values while a single writer commits data transactions, significantly boosting concurrent database throughput."
        ),
        (
            "https://structures.io/trie",
            "Trie Prefix Trees for Fast Autocomplete",
            "A Trie or prefix tree is a specialized search tree data structure. Tries store strings using character-associated nodes. Tries enable super fast prefix searching, making them the ideal index structures for typeahead auto-complete suggestions."
        ),
        (
            "https://google.org/pagerank",
            "Google PageRank: Linking Authority",
            "PageRank is the original algorithm used by Google search engine to rank web pages. PageRank measures the importance of pages by analyzing the network of directed links. A page gets high rank if it has many incoming links from other authoritative pages."
        )
    ];

    let mut stmt_doc = conn.prepare(
        "INSERT INTO documents (url, title, body) VALUES (?1, ?2, ?3)"
    )?;

    for (url, title, body) in seed_docs {
        stmt_doc.execute(params![url, title, body])?;
    }

    // 3. Seed links between documents to form a PageRank graph
    let seed_links = vec![
        // Concurrency cluster: Rust intro <-> Rust concurrency <-> Tokio Async
        ("https://rust-lang.org/intro", "https://rust-lang.org/concurrency"),
        ("https://rust-lang.org/concurrency", "https://rust-lang.org/intro"),
        ("https://rust-lang.org/concurrency", "https://tokio.rs/async"),
        ("https://tokio.rs/async", "https://rust-lang.org/intro"),
        ("https://tokio.rs/async", "https://rust-lang.org/concurrency"),
        
        // Frontend cluster: React start <-> React hooks -> JS async
        ("https://react.dev/start", "https://react.dev/hooks"),
        ("https://react.dev/hooks", "https://react.dev/start"),
        ("https://react.dev/hooks", "https://js-guide.com/async"),
        ("https://js-guide.com/async", "https://react.dev/start"),
        
        // Database & Index cluster: SQLite <-> WAL Mode -> Trie prefix
        ("https://sqlite.org/about", "https://sqlite.org/wal-mode"),
        ("https://sqlite.org/wal-mode", "https://sqlite.org/about"),
        ("https://sqlite.org/wal-mode", "https://structures.io/trie"),
        
        // Google PageRank links to all systems concepts to represent authority
        ("https://google.org/pagerank", "https://rust-lang.org/intro"),
        ("https://google.org/pagerank", "https://sqlite.org/about"),
        ("https://google.org/pagerank", "https://structures.io/trie"),
        
        // Link cross-overs: React start links to Rust intro, Tries link to SQLite WAL
        ("https://react.dev/start", "https://rust-lang.org/intro"),
        ("https://structures.io/trie", "https://sqlite.org/wal-mode"),
        ("https://structures.io/trie", "https://google.org/pagerank")
    ];

    let mut stmt_link = conn.prepare(
        "INSERT OR IGNORE INTO links (from_url, to_url) VALUES (?1, ?2)"
    )?;

    for (from_url, to_url) in seed_links {
        stmt_link.execute(params![from_url, to_url])?;
    }

    // 4. Run inverted indexer and PageRank solver on the seeded documents
    let mut stmt_fetch = conn.prepare("SELECT id, body FROM documents")?;
    let doc_rows = stmt_fetch.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;

    for doc in doc_rows {
        let (id, body) = doc?;
        crate::indexer::index_document(conn, id, &body)?;
    }

    // Solve PageRank on the linkage graph
    crate::pagerank::calculate_pagerank(conn)?;

    Ok(())
}

pub fn get_all_queries(conn: &Connection) -> Result<Vec<(String, u64)>> {
    let mut stmt = conn.prepare("SELECT query, count FROM queries ORDER BY count DESC")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get(0)?, row.get::<_, i64>(1)? as u64))
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

pub fn get_trending_queries(conn: &Connection, limit: usize, window_minutes: u32) -> Result<Vec<(String, u32)>> {
    let window_str = format!("-{} minutes", window_minutes);
    let mut stmt = conn.prepare(
        "SELECT query, COUNT(*) as trend_score 
         FROM query_logs 
         WHERE timestamp >= datetime('now', ?1)
         GROUP BY query 
         ORDER BY trend_score DESC 
         LIMIT ?2"
    )?;

    let rows = stmt.query_map(params![window_str, limit], |row| {
        Ok((row.get(0)?, row.get::<_, i64>(1)? as u32))
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}
