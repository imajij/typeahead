# Googol — Complete Code Walkthrough

A line-level guide to the entire project. It is organized so you can find any
**file → function → line** and explain what it does and why it exists. The
companion `decisions.md` explains the *design rationale* (referenced here as §N);
this document explains the *code*.

---

## Table of contents

1. [The 30-second mental model](#1-the-30-second-mental-model)
2. [Architecture & data flow](#2-architecture--data-flow)
3. [`Cargo.toml` — dependencies](#3-cargotoml--dependencies)
4. [`src/models.rs` — the wire types](#4-srcmodelsrs--the-wire-types)
5. [`src/main.rs` — boot & wiring](#5-srcmainrs--boot--wiring)
6. [`src/db.rs` — schema, seeding, queries](#6-srcdbrs--schema-seeding-queries)
7. [`src/suggest.rs` — the trie engine (the heart)](#7-srcsuggestrs--the-trie-engine-the-heart)
8. [`src/cache.rs` — concurrency wrapper](#8-srccachers--concurrency-wrapper)
9. [`src/trending.rs` — trending snapshot](#9-srctrendingrs--trending-snapshot)
10. [`src/batch.rs` — the write path](#10-srcbatchrs--the-write-path)
11. [`src/indexer.rs` — tokenizer + TF-IDF](#11-srcindexerrs--tokenizer--tf-idf-document-search)
12. [`src/pagerank.rs` — power-iteration PageRank](#12-srcpagerankrs--power-iteration-pagerank)
13. [`src/handlers.rs` — the HTTP layer](#13-srchandlersrs--the-http-layer)
14. [Frontend — `index.html` + `search.js`](#14-frontend--staticindexhtml--searchjs)
15. [Console — `console.html` + `app.js`](#15-console--staticconsolehtml--appjs)
16. [The CSS](#16-the-css-searchcss-stylecss)
17. [End-to-end traces](#17-end-to-end-traces)
18. [Quirks & known limits](#18-quirks--known-limits)

---

## 1. The 30-second mental model

This is a **single-binary search-engine simulator** with two halves that share
one SQLite file:

1. **Typeahead** (the autocomplete dropdown): an in-memory **trie** with a
   precomputed top-10 list at every node, plus Damerau–Levenshtein typo repair.
   Reads happen on every keystroke; they **never** touch SQLite.
2. **Document search** (the results page): a SQLite **inverted index** scored by
   **TF-IDF × PageRank**.

SQLite is the **durable system of record**. The trie is a **disposable index**,
rebuilt from SQLite at boot and kept fresh by a background **batch worker**. The
frontend is **vanilla JS, zero build step**, served as static files by the same
server.

---

## 2. Architecture & data flow

```
  keystroke ──GET /api/search──► trie (in-memory) ───────────────► dropdown
  enter/click ─POST /api/search─► mpsc channel ─► batch worker ──► SQLite + trie + trending
              └GET /api/query───► inverted index (SQLite) ─► TF-IDF×PageRank ─► results
  console ────POST /api/documents─► docs+links+inverted_index ─► PageRank recompute
```

**Module dependency sketch:**

```
main ──► db, cache, trending, batch, handlers, models, indexer, pagerank, suggest
handlers ──► cache, trending, indexer, pagerank, db (via spawn_blocking), models
batch ──► db, cache, trending, models
cache ──► suggest
db ──► indexer, pagerank
```

**The core split (§2):** the suggestion read path only ever touches the
in-memory engine; SQLite is only on the write path and the boot path. If the
process dies, the trie is rebuilt from SQLite — counts can never silently
diverge.

---

## 3. `Cargo.toml` — dependencies

```toml
axum = "0.7"                                            # HTTP framework: routing, extractors, JSON
tokio = { version = "1", features = ["full"] }          # async runtime: scheduler, mpsc, RwLock, time, spawn_blocking
serde = { version = "1", features = ["derive"] }        # #[derive(Serialize/Deserialize)] codegen
serde_json = "1"                                        # ad-hoc json!{} values for error bodies
rusqlite = { version = "0.31", features = ["bundled"] } # SQLite driver; "bundled" compiles SQLite in
csv = "1"                                               # declared but UNUSED in current code
tower-http = { version = "0.5", features = ["fs", "cors"] } # ServeDir (static files) + CorsLayer
chrono = { version = "0.4", features = ["serde"] }      # timestamps for log lines
```

- **`bundled`**: no external `libsqlite3` needed — SQLite is compiled from
  source into the binary.
- **`csv`**: dead weight — nothing imports it. Safe to remove.
- **`tokio` "full"**: pulls in every Tokio feature (multi-thread runtime, time,
  sync primitives, `spawn_blocking`). Convenient for a demo; a release build
  could trim to the features actually used.

---

## 4. `src/models.rs` — the wire types

Every struct here is a JSON shape crossing the HTTP boundary. `#[derive(Serialize,
Deserialize)]` makes serde generate to/from-JSON code at compile time.

| Struct | Lines | Role |
|---|---|---|
| `SearchSuggestion { query, count, fuzzy }` | 4–11 | one autocomplete row. `fuzzy: bool` is *why* the UI can render typo-corrections differently |
| `SearchRequest { query }` | 13–16 | body of `POST /api/search` |
| `SearchResponse { suggestions }` | 18–21 | body of `GET /api/search` |
| `TrendingQuery { query, score }` | 23–27 | one trending row; `score` is a recent-window count |
| `TrendingResponse { trending }` | 29–32 | body of `GET /api/trending` |
| `SearchMatch { url, title, snippet, score, tf_idf_score, pagerank_score }` | 35–43 | one document result; carries the score breakdown |
| `QueryResultResponse { results, elapsed_ms }` | 45–50 | body of `GET /api/query`; `elapsed_ms` drives "N hits · 0.003s" |
| `Document { id, url, title, body, pagerank }` | 53–60 | a stored page row |
| `DocumentIngestRequest { url, title, body, links }` | 62–68 | `POST /api/documents` body; **only `Deserialize`** (input only) |
| `SystemStats { … }` | 71–79 | everything the console dashboard polls |

Key detail: `count: u64` (all-time suggestion popularity) and `score: u32`
(recent trending count) are **different fields with different sources** — never
conflate them.

---

## 5. `src/main.rs` — boot & wiring

### `AppState` (lines 20–27)

The shared application state. `#[derive(Clone)]` matters: Axum clones it into
every handler, so all fields are cheap to clone:

- `cache: cache::Cache` — the trie (internally `Arc<RwLock<…>>`; clone = bump refcount).
- `trending_cache: trending::TrendingCache` — same shape.
- `db_path: String` — cloned per request; cheap enough at this scale.
- `search_tx: mpsc::Sender<String>` — producer end of the write queue. Cloning a
  `Sender` is how many handlers feed one worker.
- `recent_logs: Arc<Mutex<Vec<String>>>` — the rolling log buffer the console
  reads. Uses **`std::sync::Mutex`** (not Tokio's) because `log()` is synchronous
  and never held across an `.await`.

### `AppState::log` (31–39)

Takes the std mutex (`if let Ok` silently ignores poisoning), prepends a
`[HH:MM:SS] msg` line at index 0 (newest first), and `truncate(35)` caps the
buffer. Timestamp via `chrono::Local::now().format("%H:%M:%S")`.

### `main` (`#[tokio::main]`, 42–126) — boot sequence, in order

1. **46–50**: pick DB path, `create_dir_all` for `data/` and `static/`
   (idempotent — won't error if they exist).
2. **53**: `db::init_db` — opens SQLite, creates tables, **seeds on first run,
   and runs the indexer + PageRank**, so a fresh DB comes up fully populated.
3. **56–57**: load the **initial datasets** — all queries (for the trie) and the
   last-60-min trending top-10.
4. **60–61**: build the trie cache and `rebuild` it from the queries (the
   boot-only full build, §5).
5. **64–70**: build the trending cache; `.map(...)` converts DB `(String, u32)`
   tuples into `TrendingQuery` structs.
6. **73**: `mpsc::channel::<String>(2000)` — the **bounded** write queue. 2000 is
   the backpressure ceiling (§8): a full channel makes producers error rather
   than the process OOM.
7. **74**: create the shared log buffer.
8. **76–87**: assemble `AppState`; emit four cosmetic boot log lines.
9. **90–103**: **spawn the batch worker** as a background task, moving clones of
   everything it needs. The clones share the same `Arc`s, so the worker writes to
   the *same* trie the handlers read.
10. **106–119**: the **router** — the entire API surface:

| Route | Method | Handler | Meaning |
|---|---|---|---|
| `/api/search` | GET | `search_handler` | autocomplete suggestions |
| `/api/search` | POST | `record_search_handler` | record an executed search |
| `/api/trending` | GET | `trending_handler` | trending list |
| `/api/query` | GET | `query_handler` | document search (TF-IDF×PageRank) |
| `/api/documents` | GET | `list_documents_handler` | list docs by PageRank |
| `/api/documents` | POST | `ingest_document_handler` | crawl-ingest a document |
| `/api/stats` | GET | `stats_handler` | dashboard telemetry |
| `/` (nest) | — | `ServeDir("static")` | static files |

   `.layer(CorsLayer::permissive())` allows any origin (fine for a local demo).
   `.with_state(state)` injects `AppState` into every handler.
11. **121–125**: bind `127.0.0.1:3000` and serve forever. `.unwrap()` means a
    bind failure crashes at startup — acceptable for a single-node demo.

**Ordering matters**: DB seeded *before* trie loads from it; worker spawned
*before* the server accepts traffic.

---

## 6. `src/db.rs` — schema, seeding, queries

### `init_db` (3–82)

- **4**: `Connection::open` creates the file if absent.
- **7**: `pragma_update(… "journal_mode", "WAL")` — switches to Write-Ahead
  Logging so many readers + one writer don't block each other (§2). *This is the
  pragma that produces the `*.db-wal` / `*.db-shm` sidecar files.*
- **10–68**: `CREATE TABLE IF NOT EXISTS` for six tables:

| Table | Shape | Purpose |
|---|---|---|
| `queries` | `query PK, count, updated_at` | all-time popularity per normalized query |
| `query_logs` | `id, query, timestamp` | append-only event log for trending + audit |
| `documents` | `id, url UNIQUE, title, body, pagerank` | crawled pages |
| `links` | `from_url, to_url, PK(from,to)` | directed graph edges |
| `inverted_index` | `term, doc_id, term_frequency, PK(term,doc_id), FK→documents ON DELETE CASCADE` | postings |

  Two indexes: `idx_query_logs_timestamp` (28–31) makes the trending window query
  fast; `idx_inverted_index_term` (66–68) makes `WHERE term IN (…)` a single
  indexed lookup.
- **71–79**: count documents; if zero, call `seed_db`. The "first run only" gate.

### `seed_db` (84–261)

- **87–128**: 40 seed queries with realistic counts (engineered to demo prefix,
  mid-word, and typo matching).
- **130–143**: insert each query, then synthesize `query_logs` rows so trending
  isn't empty at boot.
  - **134**: `num_logs = (cnt / 25).max(1).min(20)` — between 1 and 20 log rows,
    proportional to popularity.
  - **139–142**: each log is stamped `datetime('now', '-{i*3} minutes')`,
    spreading events backward in 3-min steps so they land inside the 60-min
    trending window.
- **146–205**: 10 seed documents (real-ish tech pages). Inserted without
  `pagerank` (defaults to 0; computed later).
- **207–244**: 18 seed links forming three clusters plus cross-links, so PageRank
  has a non-trivial graph. `INSERT OR IGNORE` skips duplicate edges.
- **246–258**: **the build step** — fetch every doc, call
  `indexer::index_document` to populate the inverted index, then
  `pagerank::calculate_pagerank` once. This is why a freshly seeded DB is
  immediately searchable.

### `get_all_queries` (263–274)

`SELECT query, count FROM queries ORDER BY count DESC`. The
`row.get::<_, i64>(1)? as u64` casts SQLite's signed integer to the `u64` the
trie wants. Used at boot to build the trie.

### `get_trending_queries` (276–296)

```sql
SELECT query, COUNT(*) AS trend_score FROM query_logs
WHERE timestamp >= datetime('now', ?1)   -- ?1 = '-60 minutes'
GROUP BY query ORDER BY trend_score DESC LIMIT ?2
```

"Popular *recently*" — counting log rows in a sliding window, a fundamentally
different question than the trie's all-time count (§9). Called at boot and after
every batch flush.

---

## 7. `src/suggest.rs` — the trie engine (the heart)

The single most important file. Read it slowly.

### Types

- **`TOP_K = 10`** (4): suggestions cached per node. Matches the API's clamp ceiling.
- **`Suggestion { text, count, fuzzy }`** (7–12): the engine's output candidate.
- **`ScoredRef { id: u32, count: u64 }`** (14–18): a *reference* into the entry
  registry, not an owned string. 12 bytes. Interning keeps node lists tiny (§3).
- **`TrieNode { children: HashMap<char, TrieNode>, top: Vec<ScoredRef> }`**
  (20–27): each node maps a character to a child and holds its subtree's
  **precomputed top-K**. The whole speed story: `top` is already sorted, so
  lookups do no work.
- **`Entry { text, count }`** (29–33): the canonical record for a query; lives
  once in `entries`.
- **`SuggestEngine { root, entries: Vec<Entry>, by_text: HashMap<String,u32> }`**
  (43–48): the trie root, the entry registry (indexed by id = position), and a
  text→id map for interning. `#[derive(Default)]` gives a free empty engine.

### `normalize_query` (52–59)

The canonical form used **everywhere** (indexing *and* lookup), so keys can never
drift (§12):

```rust
raw.to_lowercase()                  // case-fold
   .split_whitespace()              // split on any run of whitespace
   .collect::<Vec<_>>().join(" ")   // re-join with single spaces (collapses runs)
collapsed.chars().take(100).collect() // cap at 100 CHARACTERS (not bytes)
```

`take(100)` caps by character, so multibyte input can't be split mid-char. Test
on line 335 pins `"  ReAcT   Hooks \t Guide "` → `"react hooks guide"`.

### Mutation: `set_count`, `increment`, `intern`, `reindex`

- **`set_count(text, count)`** (67–71): intern → set **absolute** count →
  reindex. Used at boot (DB already holds totals).
- **`increment(text, delta)`** (74–78): intern → **add** delta → reindex. Used by
  live traffic.
- **`intern(text)`** (145–156): return the existing id, or push a new
  `Entry { text, count: 0 }`, record `by_text[text]=id`, return the new id. One id
  per distinct query.
- **`reindex(id)`** (161–173) — the update algorithm:
  - **162–163**: clone the entry's text and count so it can then borrow
    `&mut self.root` without a borrow-checker conflict.
  - For **each index key** (full string + every word-boundary suffix):
    - `update_top(&mut node.top, id, count)` on the **root** (167) — root's `top`
      is the global top-K.
    - walk char by char, `children.entry(ch).or_default()` creating nodes as
      needed (169), `update_top` at every node on the path (170).
  - Because updates are keyed by **id**, re-walking shared prefix nodes is
    idempotent — a query can never appear twice in one node's list (§4). This is
    the subtlety the `duplicate_words_do_not_duplicate_suggestions` test guards.
- **`update_top`** (257–264) — per-node list maintenance:

  ```rust
  match top.iter_mut().find(|r| r.id == id) {
      Some(r) => r.count = count,                 // present → update count
      None => top.push(ScoredRef { id, count }),  // new → append
  }
  top.sort_by(|a,b| b.count.cmp(&a.count).then(a.id.cmp(&b.id))); // count desc, id asc tiebreak
  top.truncate(TOP_K);                            // keep only top 10
  ```

  The `.then(a.id.cmp(&b.id))` makes ranking **deterministic** on ties (lower
  insertion id wins). Truncation is safe because counts only ever increase
  (monotonicity, §5): an entry pushed out can only return by growing past the
  current 10th, which the next `update_top` handles.

### `index_keys` (244–255)

Generates the keys a query is inserted under:

```rust
let mut keys = vec![text];                 // the whole query
for (i, ch) in text.char_indices() {
    if ch == ' ' {
        let suffix = &text[i + 1..];       // everything after this space
        if !suffix.is_empty() && !keys.contains(&suffix) { keys.push(suffix); }
    }
}
```

`"react hooks guide"` → `["react hooks guide", "hooks guide", "guide"]`, all
pointing at the same entry id. This is **word-boundary suffix indexing** — it lets
typing `hooks` surface `react hooks guide` (§4). `!keys.contains` dedups repeated
suffixes (e.g. `"go go go"`). `char_indices` gives byte offsets, so `&text[i+1..]`
is always a valid UTF-8 boundary.

### Read path: `suggest` (82–135)

1. **83–85**: empty prefix or `limit==0` → empty (guard).
2. **87–100**: **exact + mid-word.** `walk(prefix)` descends to the prefix node;
   its `top` is *already* the sorted top-K of everything under it. Copy entries
   in, marking `fuzzy: false`, deduping via `seen: HashSet<id>`. O(prefix length)
   + a ≤10 clone.
3. **102–131**: **fuzzy fill**, only if still under `limit`:
   - **103–107**: edit budget scales with prefix length — `0` for ≤2 chars, `1`
     for 3–5, `2` for 6+. Short prefixes get no fuzzing (too little signal;
     `"zz"` returns nothing — the `short_prefixes_skip_fuzzy` test).
   - **108**: skip entirely if budget is 0.
   - **110**: `fuzzy_collect` fills `HashMap<id, (count, dist)>`.
   - **112–116**: drop ids already in `seen` (no fuzzy dup of an exact hit).
   - **118**: sort fuzzy by `(distance asc, count desc, id asc)` — closest first,
     popularity breaks ties, id is the final deterministic tiebreak.
   - **120–129**: append until `limit`, marking `fuzzy: true`.
4. **133**: `truncate(limit)` final cap.

Ordering contract: **exact/mid-word (by popularity) first, then fuzzy (by
closeness then popularity)** — exactly what §6/§7 promise.

### `walk` (175–181)

Plain trie descent: `children.get(&ch)?` returns `None` the moment a character
isn't found. Returns the node at the end of the prefix.

### Typo tolerance: `fuzzy_collect` + `fuzzy_dfs` (185–239)

A **Damerau–Levenshtein automaton walked over the trie**, carrying one DP row per
node — no separate index (§6).

**`fuzzy_collect`** (185–191): `pattern` = the typed prefix as a `Vec<char>`.
`first_row = [0,1,2,…,len]` is the base DP row (edit distance from the empty
string to each prefix of the pattern). Kicks off a DFS from each root child.

**`fuzzy_dfs`** (193–239) — for a node reached by character `ch`:

- **204–220** build `row`, the edit-distance row between *this node's path string*
  and every prefix of the pattern:
  - **205**: `row[0] = prev_row[0] + 1` — matching an empty pattern against a path
    of this depth costs the depth (delete every path char).
  - **206–219** for each `i`:
    - `cost = 0 if pattern[i-1]==ch else 1` (substitution cost),
    - classic three-way min:
      - `row[i-1]+1` — **insertion**,
      - `prev_row[i]+1` — **deletion**,
      - `prev_row[i-1]+cost` — **match/substitute**.
    - **212–218 transposition (Damerau / OSA):** if
      `pattern[i-1]==prev_ch && pattern[i-2]==ch`, an adjacent swap costs one
      edit: `d = min(d, prev_prev_row[i-2] + 1)`. `prev_prev_row` is the
      **grandparent's** row — that's why the function threads two rows of history.
      This is why `raect → react` is 1 edit, not 2 (§6); the
      `fuzzy_corrects_typos` test pins both substitution and transposition.
- **222–230**: `dist = row[pattern.len()]` is the edit distance between the full
  path string and the full typed prefix. If `dist <= max_edits`, the node's path
  is "close enough," so harvest its precomputed `top` into `found`, keeping the
  best `(count, dist)` per id (min distance, then max count).
- **233–238 — the pruning that makes this fast:** only recurse into children
  while `min(row) <= max_edits`. The row minimum is a lower bound on what any
  deeper path can achieve, so once it exceeds budget the whole subtree is
  hopeless. This guarantees the DFS terminates quickly instead of scanning the
  entire trie.

**Worked example (`raect`, budget 1):** the DFS reaches the node at path `react`
via the transposition rule (`a`/`e` swapped), computes `dist = 1 ≤ 1`, and
harvests `react`'s subtree top-K. Fuzzy matching reuses the *same* trie, the
*same* suffix keys, and the *same* precomputed top-K as exact matching — even a
typo'd middle word gets corrected because suffix-key nodes are in the same
structure.

### `node_count` (138–143)

Recursive node tally (`1 + sum of children`) for the stats endpoint. Pure
diagnostic.

### Tests (266–338)

Eight tests pin the behavioral contract: prefix ranking, mid-word matching,
substitution + transposition fuzzy, no-fuzzy-on-short-prefixes, in-place
re-ranking after `increment`, suffix dedup, normalization. These are the
guardrails if you change the engine.

---

## 8. `src/cache.rs` — concurrency wrapper

The engine in §7 is a plain single-threaded data structure. This file wraps it
for concurrent access (§10):

- **`Cache { engine: Arc<RwLock<SuggestEngine>> }`** (5–8): `Arc` = shared
  ownership across tasks; `RwLock` = many readers OR one writer. **Tokio's** async
  `RwLock`, so locking `.await`s instead of blocking a runtime thread.
- **`rebuild(queries)`** (19–25): build a brand-new engine from absolute counts,
  then `*self.engine.write().await = engine` atomically swaps it in. **Boot only.**
- **`apply(deltas)`** (29–34): take the write lock once, `increment` each delta in
  place. The live update path — microseconds under lock, no rebuild (§5). Called
  by the batch worker after a successful DB commit.
- **`get_suggestions(prefix, limit)`** (37–39): take a **read** lock, call
  `suggest`. Concurrent keystrokes don't block each other.
- **`count_nodes`** (42–44): read-locked node tally for stats.

`RwLock` over `Mutex` because reads dominate massively (every keystroke); a
`Mutex` would serialize them (§10).

---

## 9. `src/trending.rs` — trending snapshot

Trivial by design: `TrendingCache { queries: Arc<RwLock<Vec<TrendingQuery>>> }`.

- **`update(new)`** (18–21): write-lock, replace the whole vec.
- **`get_trending`** (24–27): read-lock, clone the vec out.

The point (§9): `GET /api/trending` is a **pure memory read** of a snapshot
refreshed once per batch flush — not a SQL query per request.

---

## 10. `src/batch.rs` — the write path

The background worker that turns a stream of recorded searches into durable
counts + a fresh trie + fresh trending, **batched** so the DB and trie aren't hit
per event (§8).

### Constants (10–13)

Flush every `2s` **or** at `50` buffered events; trending = top `10` over `60` min.

### `start_batch_worker` (18–75)

- **25–33**: a local `log_fn` closure that writes into the shared log buffer.
  (Note: it `truncate(30)`, while `AppState::log` truncates to 35 — a minor
  inconsistency; both write the same Vec.)
- **37**: `buffer: HashMap<String, u64>` — accumulates per-query deltas in memory.
- **38–39**: a `2s` interval timer; `MissedTickBehavior::Delay` means if a flush
  runs long, the next tick is delayed rather than firing immediately to "catch up"
  (avoids a burst).
- **41–74**: the **event loop** built on `tokio::select!` (45–62) — waits on
  *whichever happens first*:
  - **a queued query arrives** (`rx.recv()`, 46–51):
    `*buffer.entry(query).or_insert(0) += 1`; `flush_now` becomes true if total
    buffered events hit 50.
  - **the channel closes** (`None`, 52–57): producers all dropped → flush
    whatever's left and set `shutting_down`.
  - **the timer ticks** (59–61): flush if the buffer is non-empty.
  - **64–69**: if `flush_now`, call `flush_batch`; **only `clear()` on success** —
    on failure the deltas are retained and retried next tick (idempotent upserts
    make retry safe, §8).
  - **71–73**: break the loop on shutdown.

### `flush_batch` (79–143)

- **88–90**: snapshot the buffer into a `Vec<(query, delta)>` and clone it (one
  copy for the DB closure, one for the trie).
- **92–116**: `spawn_blocking` — rusqlite is synchronous, so blocking DB work runs
  on Tokio's blocking pool, not the async threads (§2). Inside:
  - open a connection, begin a **transaction** (one fsync amortized over the
    whole batch).
  - **96–102**: the **upsert** —
    `INSERT … ON CONFLICT(query) DO UPDATE SET count = count + ?2`. New query →
    inserted with its delta as initial count; existing → count incremented.
  - **103–110**: also append to `query_logs`, **one row per event**
    (`for _ in 0..*count`), so the trending window reflects true recent volume.
  - **112**: commit.
  - **114**: while still on the blocking thread, recompute trending and return it.
- **118–142**: handle the result:
  - **success** (119–133): `cache.apply(&deltas)` does the **incremental trie
    update** (the same deltas committed to SQLite — so memory can't diverge from
    disk, §8), then `trending_cache.update(...)` swaps in fresh trending. Return
    `true`.
  - **DB error** (134–137) or **join error** (138–141): log and return `false` →
    buffer retained, retried.

The invariant to be able to explain: the trie is updated **only** through deltas
already committed to SQLite, and boot reloads from SQLite — so in-memory counts
are provably consistent with durable ones.

---

## 11. `src/indexer.rs` — tokenizer + TF-IDF document search

### `tokenize` (7–16)

lowercase → replace every non-alphanumeric char with a space → split on
whitespace → keep tokens of length ≥ 2 → own them as `String`s. Punctuation
becomes word boundaries; single-letter noise is dropped. Used for **both**
indexing and querying (same function = same tokens both sides).

### `index_document` (20–40)

tokenize the body, count term frequencies into a `HashMap` (23–25), `DELETE` any
existing postings for this doc (28 — makes re-ingest idempotent), then
bulk-`INSERT` one `(term, doc_id, tf)` row per distinct term (31–38).

### `search_and_rank` (44–132) — the ranking pipeline

1. **45–48**: tokenize the query; empty → no results.
2. **51–54**: fetch `N` = total document count; zero → no results.
3. **57–72**: build a parameterized `WHERE term IN (?1,?2,…)` (placeholders
   generated to match term count, 57–65) and fetch all postings for the query
   terms in **one** indexed query. `params_from_iter` binds the term list.
4. **75–79**: group postings by term into `HashMap<term, Vec<(doc_id, tf)>>`.
5. **82–87**: **IDF per term** — `((N - df + 0.5)/(df + 0.5)).max(0.0001).ln()`,
   the **BM25 IDF form** (§11). `df` = number of docs containing the term
   (= postings length). `.max(0.0001)` guards `ln` of a non-positive value.
   ⚠️ This form can go **negative** for a term appearing in almost every document
   — standard BM25 behavior; at this tiny corpus it occasionally down-weights
   ultra-common words.
6. **90–97**: **accumulate TF-IDF per doc** — `score += tf * idf` summed across
   the query's matching terms.
7. **100–126**: for each scoring doc, fetch metadata + pagerank, compute the
   **combined score** `tf_idf * (1.0 + 10.0 * pagerank)` (114) — multiplicative
   blend so authority is a multiplier on relevance, with `10×` making a
   normalized (sums-to-1) PageRank meaningful (§11). Build a snippet, push a
   `SearchMatch`.
8. **129–131**: sort by combined score descending. `partial_cmp(...).unwrap_or(
   Equal)` handles `f64` not being totally ordered (NaN safety).

### `create_snippet` (135–159)

find the first query term in the body (lowercased search, 136–144), take a
~150-char window starting ~45 chars before it (147–149), and prepend/append `…`
if truncated (151–156). The `.min(body.len())` and `if start > 45` checks keep the
slice in bounds. ⚠️ Subtle: slicing uses **byte** offsets from a lowercased copy
applied to the original `body` — fine for ASCII (all seed content), but a
multibyte body could panic on a non-char-boundary slice. Known limitation at this
scale.

---

## 12. `src/pagerank.rs` — power-iteration PageRank

### `calculate_pagerank` (6–94)

Exact PageRank over the link graph, recomputed synchronously on every ingest (§11
— ingest is rare/admin, so it can be the slow path).

1. **8–24**: load docs and build three maps — `id_to_index`, `index_to_id` (vec),
   `url_to_index`. PageRank works on dense 0..n indices; these translate to/from
   DB ids and URLs.
2. **26–28**: empty graph → return.
3. **31–48**: load links, build `out_degree[]` and `in_links[][]` (incoming
   adjacency). **43–46 dedup** parallel edges so a doubled link doesn't inflate
   out-degree.
4. **51–77**: **power iteration** — damping `0.85`, `20` fixed iterations, start
   uniform `1/n`:
   - **59–64 sink mass**: sum the rank of dangling nodes (no out-links); their
     rank would otherwise leak out of the system.
   - **67**: `base_value = (1-d)/n + d * (sink_sum/n)` — the teleport term plus
     redistributed sink mass spread evenly.
   - **69–75**: each node's new rank =
     `base_value + d * Σ(pr[incoming]/out_degree[incoming])` — every in-linker
     passes a share of its rank divided by how many pages it links to.
   - **76**: swap `pr = next_pr`.
5. **80–85**: **normalize** so ranks sum to 1.0 (clean percentages for display).
6. **88–91**: write each rank back to `documents.pagerank`.

20 iterations converge plenty at this corpus size.

---

## 13. `src/handlers.rs` — the HTTP layer

`SearchParams { q: Option<String>, limit: Option<usize> }` (13–17) is the shared
query-string extractor for the GET endpoints.

### `search_handler` — GET /api/search (22–45)

normalize `q` (26 — same `normalize_query` as indexing, so lookups can't drift);
empty → empty list; **clamp `limit` to 1..=10** (31 — never trust the client with
unbounded fan-out, §12); read suggestions from the cache and map engine
`Suggestion` → wire `SearchSuggestion`. Pure read, no logging.

### `record_search_handler` — POST /api/search (49–77)

normalize; empty → `400`. Then `search_tx.send(query).await` enqueues onto the
bounded channel: success → log + **`202 Accepted`** (honest status: "queued, not
yet durable", §8); send error (channel full) → log + `500`. **The only place
searches enter the write path.**

### `query_handler` — GET /api/query (90–124)

document search. Trim `q` (uses `.trim()`, not `normalize_query` — the tokenizer
normalizes anyway). Empty → `200` with empty results. **101**: `Instant::now()`
starts the timer; **108**: `elapsed().as_secs_f64()*1000.0` is the `elapsed_ms`
the UI shows. Ranking runs in `spawn_blocking` (104–107). Errors → `500` + empty.
This handler does **not** record the query — recording is the client's separate
POST.

### `ingest_document_handler` — POST /api/documents (128–209)

validate non-empty url/title/body (136–141). In `spawn_blocking` (148–193):

- upsert the document (`ON CONFLICT(url) DO UPDATE`, 155–160), fetch its id
  (162–166).
- **replace** its links: `DELETE FROM links WHERE from_url=?` then re-insert
  (169–180), skipping empty and self-links (177).
- commit, then **re-index** the doc (186–187) and **recompute PageRank for the
  whole graph** (190). Returns `201 Created`.
- ⚠️ The indexer/PageRank run on a *second* connection opened after commit (186),
  outside the transaction — fine here, but index+pagerank aren't atomic with the
  doc insert.

### `list_documents_handler` — GET /api/documents (213–243)

`SELECT … ORDER BY pagerank DESC` in `spawn_blocking`; map rows to `Document`. Any
error → `500` + empty.

### `stats_handler` — GET /api/stats (247–287)

assembles `SystemStats`. **250**: `queue_size = max_capacity - capacity` — the
messages currently buffered in the mpsc channel (2000 minus remaining slots).
**251**: live trie node count. **254–257**: DB file size from `fs::metadata`.
**261–269**: row counts in `spawn_blocking` (with `unwrap_or(0)` fallbacks).
**273–277**: clone the log buffer. ⚠️ `database_size_bytes` *is* returned but the
console never displays it (see §15).

---

## 14. Frontend — `static/index.html` + `search.js`

The polished, user-facing search page. This is "the hard part of autocomplete UIs
done right" (§13).

### `index.html` — notable lines

- **8**: `<meta name="color-scheme" content="dark">` — dark-only; the browser
  styles form controls/scrollbars dark (no light variant maintained).
- **12**: favicon is an **inline SVG data-URI** (a `>_` prompt glyph) — zero extra
  request.
- **17–21**: animated background orbs (decorative, `aria-hidden`).
- **34–49**: the search zone — a `role="combobox"` input (37–39) with full ARIA
  wiring (`aria-expanded`, `aria-controls`, `aria-autocomplete`) and a clear
  button that starts `hidden` (40).
- **46–48**: the suggestion panel (`hidden` initially) holding
  `<ul role="listbox">`.
- **51–54**: the `./search` and `./lucky --random` buttons.
- **56–65**: trending chips, did-you-mean line, results meta, results `<ol>` — all
  populated by JS.

### `search.js`

**DOM handles + constants** (6–23): `DEBOUNCE_MS = 110` (below perception, above
per-keystroke spam), `SUGGEST_LIMIT = 8`, `HISTORY_KEY/MAX` for localStorage.
`ICONS` (25–30) are inline SVG strings per row type.

**State** (34–39): `items` (rendered rows), `selectedIndex`, `typedText` (what the
user actually typed — restored on Esc), `debounceTimer`, `suggestAbort` (the
AbortController), `trendingCache` (30s client cache).

**Utilities:**

- `escapeHtml` (43–46) / `escapeRegExp` (48–50): XSS/regex safety for user text
  injected into `innerHTML`.
- `normalize` (52–54): the **client mirror** of the server's `normalize_query`
  (lowercase, collapse whitespace) — used for stale-response comparisons.
- `formatCount` (56–60): `1500 → "1.5k"`, `2_000_000 → "2M"`.
- **`emphasize`** (64–74): Google's bold-the-untyped-part. Finds the typed
  substring in the suggestion and bolds **everything except** the matched part. If
  the typed text isn't a substring (a fuzzy correction), bolds the whole thing.
  Client-side, so no per-suggestion markup is shipped on every keystroke (§12).

**History** (78–93): `getHistory` reads+parses localStorage (with `try/catch`
returning `[]` on corruption); `addToHistory` dedups then unshifts then caps to
10; `removeFromHistory` filters one out. All client-side — the server stays
user-agnostic (§13).

**Trending** (97–119): `getTrending` returns the 30s-cached list or refetches
`/api/trending` (keeps stale data on network error). `renderTrendingChips` builds
chip buttons; each runs the search on click.

**Dropdown** (123–204):

- `openPanel`/`closePanel` (123–136): toggle visibility + ARIA + reset selection.
- **`renderItems(sections, typed)`** (138–192): rebuilds the list from sections
  (each with an optional label + entries). Per entry: an `<li role="option">`, an
  icon by type (history=clock, trending/suggest, fuzzy=wand), and the **right-side
  affordance** — `~fixed` tag for fuzzy, formatted count for normal, a **Remove**
  button for history (166–169). `mousemove` selects *without* filling the input
  (173); `click` runs the search (174). The Remove button `stopPropagation`s
  (177–183).
- **`setSelected(idx, {fillInput})`** (194–204): the keyboard-nav core. Toggles
  `.selected`/`aria-selected`, sets `aria-activedescendant`, and **fills the
  input** with the selected text — where `idx === -1` restores `typedText` (the
  "nothing selected = your typed text" state).

**`showZeroState`** (207–218): the empty-but-focused dropdown — recent history (up
to 6) + trending (up to 5, minus anything already in history). **213** is a stale
guard: if the box went non-empty or lost focus while awaiting trending, bail.

**`refreshSuggestions`** (220–260) — the live suggestion fetch, the most important
function for race-correctness:

- empty query → zero-state.
- **230–231**: **abort the previous in-flight fetch** before starting a new one
  (`AbortController`).
- **233–242**: fetch `/api/search?q=…&limit=8` with the abort signal; on abort,
  swallow the error.
- **244 — the stale-response guard:** `if normalize(input.value) !== q return`.
  Even with abort, a response already in the JS task queue could render late; this
  compares the response's query against the *current* input and drops it if they
  differ. Belt-and-suspenders (§13).
- **246–259**: merge up to 2 **history matches** (that contain the query, aren't
  exact dups of server results) ahead of server suggestions; map server results to
  entries tagging `fuzzy` rows; render.

**Search execution** (264–312):

- `setView` (264–267): toggles `home`/`serp` body classes (CSS swaps layouts).
- **`runSearch(query, {push})`** (269–312): normalize; close panel; fill input;
  switch to SERP view; set title. **280–283: URL as state** —
  `history.pushState({q}, '', '?q=…')` so results are linkable and back/forward
  work (§13). **285**: add to history. **288–292: fire-and-forget POST**
  `/api/search` — *this* is what feeds the engine (executed searches become future
  suggestions). **294–297**: loading animation. **300–306**: fetch `/api/query`.
  **308**: another stale guard. Render results + maybe-did-you-mean.

**`renderResults`** (314–372): builds the SERP. `secs` from `elapsed_ms` (317).
Zero results → a helpful "try…" block (319–330). Otherwise the meta line
(332–333), then per match: parse the URL into domain + breadcrumb path (340–345,
with `try/catch` for non-URLs), a colored favicon letter (347), **bold the query
terms in the snippet** via regex (349–352), and the result card with title link
(`target=_blank rel=noopener`) and the `score/tfidf/pr` breakdown (366–368).

**`maybeShowDidYouMean`** (376–393): re-queries `/api/search?…&limit=3`; if the
**best** suggestion for the *full query* is `fuzzy` and differs from the query,
render a "did you mean: X" link (own stale guard on 381). No dedicated endpoint —
the suggest engine *is* the spell-checker (§13).

**Navigation** (397–415): `goHome` resets to the home view and pushes the bare
path; `applyLocation` reads `?q=` and either runs that search (no push) or goes
home — this is what makes reload/deep-link land on the right view.

**Event wiring** (419–490):

- `input` (419–423): show/hide clear button + **debounce** `refreshSuggestions` by
  110ms.
- `focus` (425–428): zero-state if empty, else refresh.
- **`keydown`** (430–448): ↓/↑ move selection with wraparound through the `-1`
  "your text" state (433–438); **Esc** restores `typedText` and closes (439–443);
  **Enter** runs the search (444–447).
- `submit` (450–453): prevent default form submit, run search.
- `clear` (455–461), click-outside-closes (463–465), and **`/` focuses the box
  from anywhere** (467–472, unless already typing in a field).
- `brandHome` (474–477), `btn-search` (479), **`btn-lucky`** (481–488, picks a
  random trending query), `popstate` (490) wires browser back/forward to
  `applyLocation`.
- **494**: `applyLocation()` — the boot call that renders the initial view from
  the URL.

---

## 15. Console — `static/console.html` + `app.js`

A **separate page** (`/console.html`) for inspecting the backend. Uses
FontAwesome (CDN) and `style.css`. Independent from the search page — they share
only the backend API.

### `console.html`

Three panels: **left** = a search ranker console (query input + autocomplete +
ranked results), **middle** = the crawler ingest form (URL/title/body/links) + a
telemetry simulator (surge/single buttons), **right** = four stat cards (SQLite
rows, indexed docs, write queue + gauge, trie nodes), the PageRank directory
table, and a live log terminal. Each element has the ids `app.js` binds to.

### `app.js`

- **Boot** (45–52): wire listeners, fetch stats + documents once, then **poll
  `/api/stats` every 1.5s**.
- **`handleTyping`** (87–106): on input, fetch `/api/search?q=…` (note: **no
  debounce** — the console is a dev tool) and render the dropdown.
- **`renderDropdown`** (109–152): builds suggestion rows, highlighting the matched
  substring via `<span class="matched">`, showing
  `formatCount(count) + " suggestions"`. Click fills + executes.
- `hideDropdown` (154–157), **`handleKeyDown`** (160–188): ↓/↑ with **modulo
  wraparound** (`(i+1)%len`), Enter runs the selected-or-typed query, Esc hides.
  `updateSelectedSuggestion` (190–199) toggles `.selected` and `scrollIntoView`.
- **`executeQuerySearch`** (202–229): fire-and-forget POST to record the query,
  then GET `/api/query` and render scorecards. `renderQueryResults` (232–268)
  shows url/title, the three score badges (with explanatory `title=` tooltips),
  and the snippet.
- **`handleDocIngestion`** (271–315): parse the comma-separated links, POST
  `/api/documents`, disable the button with a spinner, and on success clear the
  form + refresh stats and the directory.
- **`fetchDocumentsDirectory`/`renderDocumentsDirectory`** (318–350): GET
  `/api/documents`, render the PageRank table (rank as a percentage).
- **`fetchStats`** (353–378): GET `/api/stats`, update the four stat numbers,
  drive the **queue gauge** (`queue_size/50 * 100%` width — 50 being the flush
  threshold), render logs, refresh the directory. ⚠️ **364** guards
  `if (statDbSize)` — and `statDbSize` (`getElementById('stat-db-size')`, line 22)
  is **null** because no element with that id exists in `console.html`. So
  `database_size_bytes` is fetched but **never shown** — harmless dead code.
- **`renderLogs`** (381–410): only re-render if the newest line changed (382 —
  avoids flicker); parse `[HH:MM:SS] msg` and class lines as `event` if they
  mention Flushing/rebuilt/indexed/PageRank.
- **Simulations** (413–458): `triggerSingleSimulation` POSTs one random term;
  **`triggerSurgeSimulation`** fires **200** POSTs ~6ms apart (446) to exercise
  backpressure, temporarily polling stats every 200ms (434) so you can *watch* the
  queue gauge fill and the worker drain it, then restoring 1.5s polling.
- **Utilities** (461–491): `escapeHtml`, `formatCount`, `formatBytes` (the bytes
  formatter that would have shown DB size), `sleep`.

---

## 16. The CSS (`search.css`, `style.css`)

~1,350 lines of presentation. You don't need every rule, but a few carry real
intent (mostly in `search.css`):

- **`[hidden] { display: none !important; }`** (search.css:38) — a deliberate,
  documented fix (§13): any author `display:` rule silently defeats the HTML
  `hidden` attribute, which had caused the clear button to show on an empty box.
  The `!important` restores `hidden`'s authority.
- **Drifting orbs + scanlines** (`@keyframes drift-a/b/c` ~92–94; scanline overlay
  ~96): the terminal-glassmorphism aesthetic.
- **`backdrop-filter: blur(...) saturate(...)`** (repeated, e.g. 189–190) with
  `-webkit-` fallback — the frosted-glass panels.
- **`@media (prefers-reduced-motion: reduce)`** (461) — disables animation for
  users who ask for it (accessibility, §13).
- **Body `.home` vs `.serp`** classes — toggled by `setView` in JS to swap between
  the centered home layout and the results layout.

`style.css` is the console's theme — same dark/glass language, plus the stat-card
grid, the queue gauge (`.queue-progress-bar` width set inline by JS), and the log
terminal styling. No logic lives in CSS.

---

## 17. End-to-end traces

**A keystroke (`re`):** `search.js` debounces 110ms → aborts any prior fetch →
`GET /api/search?q=re&limit=8` → `search_handler` normalizes, clamps limit, calls
`cache.get_suggestions` → read-locks the trie, `walk("re")`, returns that node's
precomputed top-K (plus fuzzy fill if short) → JSON → stale-guard check →
`emphasize` bolds the untyped part → dropdown renders. **No SQLite touched.**

**An executed search (`rust`):** `runSearch` → `pushState` + history +
fire-and-forget `POST /api/search` → `record_search_handler` enqueues onto the
mpsc channel, returns `202` → batch worker accumulates the delta → on the next 2s
tick (or at 50 events) `flush_batch` opens a transaction, upserts
`queries.count += 1`, appends to `query_logs`, commits, then `cache.apply`
increments the trie in place and refreshes trending. Meanwhile `runSearch` also
`GET /api/query?q=rust` → `query_handler` times it, runs `search_and_rank` in
`spawn_blocking` (postings → IDF → TF-IDF → ×PageRank → sort) → results render
with the `N hits · 0.003s` line.

**A document ingest (console):** `handleDocIngestion` → `POST /api/documents` →
`ingest_document_handler` upserts the doc, replaces its links, commits,
re-indexes its postings, recomputes PageRank for the whole graph → `201` → console
refreshes the directory and stats.

---

## 18. Quirks & known limits

- `csv` dependency is unused.
- BM25 IDF can be **negative** for near-ubiquitous terms — by design, occasionally
  visible at 10 docs.
- `create_snippet` slices by **byte** offset; safe for the ASCII seed corpus,
  could panic on multibyte bodies.
- `database_size_bytes` is computed and returned but **never displayed** (missing
  `stat-db-size` element in `console.html`).
- Two log writers (`AppState::log` caps 35, batch `log_fn` caps 30) share one
  buffer — cosmetic mismatch.
- Buffered-but-unflushed events are **lost on crash** (≤2s/50-event window) — a
  deliberate trade (§8); the trie reloads consistently from SQLite at boot
  regardless.
- The `data/typeahead.db-wal` / `-shm` files are SQLite's WAL sidecars — they
  shouldn't be committed (a `.gitignore` candidate).
- Ingest's index + PageRank recompute run outside the document-insert transaction
  (on a second connection) — not atomic with the insert.

See `decisions.md` for the full rationale behind every choice above, plus the
scaling pressure table (§14 there) describing what breaks first and the upgrade
path.
