# Typeahead Backend (Googol Search Simulator)

A single-node, Google-style search backend in Rust: per-keystroke autocomplete
served from an in-memory trie, plus a small document search engine ranked by
TF-IDF × PageRank. Live query traffic is learned continuously through a
batched write path so suggestions stay current without a full rebuild.

Built on **Axum + Tokio + SQLite (WAL)**. Reads never touch disk; writes are
coalesced in the background.

> Deep dives live in [`decisions.md`](decisions.md) (why each choice was made)
> and [`WALKTHROUGH.md`](WALKTHROUGH.md) (line-level code tour). This README is
> the operational entry point.

---

## Architecture

```
                              ┌────────────────────────────────────────────┐
   Browser / client           │              Axum HTTP server              │
        │                     │           (127.0.0.1:3000)                 │
        │  GET /api/search    │                                            │
        ├────────────────────►│  search_handler ──► Cache.get_suggestions  │
        │  (autocomplete)     │                         │                  │
        │                     │             ┌───────────▼──────────────┐   │
        │                     │             │  Trie SuggestEngine       │   │   READ PATH
        │  GET /api/trending  │             │  Arc<RwLock<…>>           │   │   100% in-memory,
        ├────────────────────►│  trending ──┤  • top-K per node        │   │   no SQLite touch
        │                     │             │  • Damerau-Levenshtein    │   │
        │                     │             └───────────▲──────────────┘   │
        │                     │                         │ apply(deltas)     │
        │  POST /api/search   │   ┌──────────────┐  mpsc │  (in place)      │
        ├────────────────────►│──►│ search_tx    │──────►│                  │
        │  (log a query)      │   │ channel(2000)│   ┌───┴───────────────┐  │   WRITE PATH
        │   202 Accepted      │   └──────────────┘   │  Batch worker     │  │   coalesced,
        │                     │                      │  flush every 2s   │  │   off the hot path
        │                     │                      │  or 50 buffered   │  │
        │                     │                      └───────┬───────────┘  │
        │  GET  /api/query    │   spawn_blocking             │ 1 txn        │
        │  POST /api/documents│   ┌──────────────────────────▼───────────┐  │
        ├────────────────────►│──►│            SQLite (WAL)              │  │   SYSTEM OF
        │  GET  /api/documents│   │  queries · query_logs · documents   │  │   RECORD
        │  GET  /api/stats    │   │  links · inverted_index             │  │
        │                     │   └──────────────────────────────────────┘ │
        └─────────────────────┴────────────────────────────────────────────┘
       static/ (search UI + console) served from /
```

**Two independent paths.** The autocomplete *read* path is a pure in-memory
trie walk — it never reads SQLite. The *write* path (logging searches) is
fire-and-forget: the handler drops the query on an mpsc channel and returns
`202` immediately; a background worker batches the writes and patches the trie
in place. Document search (`/api/query`) is a separate, disk-backed path run on
a blocking thread pool.

---

## Setup

Requires a Rust toolchain (stable, edition 2021). SQLite is bundled via
`rusqlite` — no system library needed.

```bash
cargo build --release
cargo run --release          # listens on http://127.0.0.1:3000
```

On first boot the server creates `data/` and `static/`, opens SQLite in WAL
mode, and **auto-seeds an empty database** (see below). Open
<http://127.0.0.1:3000> for the search UI, or `/console.html` for the live
telemetry console.

Run the unit tests (trie/fuzzy/normalization logic):

```bash
cargo test
```

---

## Dataset: source & loading

There is **no external dataset file** — data is seeded programmatically and
grown by traffic.

| Source | What | When |
|---|---|---|
| `db.rs::seed_db` (in code) | 40 autocomplete phrases, 10 mock tech documents, 18 inter-doc links | Auto-runs on boot **only if `documents` is empty** |
| Live traffic | Every `POST /api/search` increments a query's count and feeds trending | Continuous |
| Bulk test seed (below) | ~5000 generated general queries for load/UX testing | Run on demand |

**Seeding is idempotent on boot:** `init_db` checks `SELECT COUNT(*) FROM
documents` and seeds only when zero. Delete `data/typeahead.db*` to force a
fresh reseed.

**Loading the 5000-row test set** (deterministic, dedup-safe — re-runnable):

```bash
python3 - <<'PY'
import random, sqlite3
random.seed(42)  # stable across reruns
topics    = ["weather","rust","python","javascript","react","news","recipe","music","movie","stock",
             "crypto","football","cricket","yoga","coffee","travel","flight","hotel","car","laptop",
             "phone","camera","game","book","history","science","math","biology","chemistry","physics",
             "docker","kubernetes","linux","git","sql","mongodb","redis","aws","azure","gcp",
             "guitar","piano","painting","photography","gardening","cooking","baking","running","cycling","swimming"]
modifiers = ["best","cheap","top","how to","what is","tutorial","guide","near me","for beginners","2026",
             "reviews","tips","ideas","online","free","vs","comparison","price","download","app",
             "course","example","problems","jobs","salary","meaning","explained","quotes","images","near you"]
tails     = ["today","this week","step by step","in india","with examples","quick","easy","advanced","pdf","video",""]
seen, rows = set(), []
while len(rows) < 5000:
    q = " ".join(x for x in [random.choice(modifiers), random.choice(topics), random.choice(tails)] if x).strip()
    if q in seen: continue
    seen.add(q); rows.append((q, random.randint(1, 500)))
con = sqlite3.connect("data/typeahead.db")
con.executemany("INSERT OR IGNORE INTO queries(query,count) VALUES (?,?)", rows)
con.commit(); con.close()
PY
```

Inserted rows surface in autocomplete after the next server start (the trie is
rebuilt from `queries` on boot). `INSERT OR IGNORE` dedups on the `query`
primary key, so re-running never double-counts.

---

## API

Base URL `http://127.0.0.1:3000`. All bodies/responses are JSON.

| Method | Route | Purpose |
|---|---|---|
| `GET`  | `/api/search?q=&limit=` | Autocomplete suggestions (prefix + mid-word + fuzzy) |
| `POST` | `/api/search` | Log a searched query (fire-and-forget) |
| `GET`  | `/api/trending` | Top trending queries (last 60 min) |
| `GET`  | `/api/query?q=` | Document search ranked by TF-IDF × PageRank |
| `GET`  | `/api/documents` | All indexed documents, by PageRank |
| `POST` | `/api/documents` | Ingest/crawl a document, re-index, recompute PageRank |
| `GET`  | `/api/stats` | System telemetry (counts, trie size, queue, logs) |

### GET /api/search

Autocomplete. `q` is normalized (lowercased, whitespace-collapsed, ≤100 chars).
`limit` defaults to **8**, clamped to **1–10**. Returns exact prefix and
mid-query word matches first (`fuzzy:false`), then typo-tolerant matches fill
any remainder (`fuzzy:true`). Fuzzy budget scales with prefix length: ≤2 chars →
none, 3–5 → 1 edit, 6+ → 2 edits (Damerau-Levenshtein, so a transposition is one
edit).

```bash
curl 'http://127.0.0.1:3000/api/search?q=react&limit=5'
```
```json
{"suggestions":[
  {"query":"react js tutorial","count":2121,"fuzzy":false},
  {"query":"react hooks guide","count":1511,"fuzzy":false}
]}
```

### POST /api/search

Records a query for popularity + trending. Normalizes, enqueues on the batch
channel, returns immediately. Does **not** wait for the DB write.

```bash
curl -X POST 'http://127.0.0.1:3000/api/search' \
  -H 'Content-Type: application/json' -d '{"query":"rust async await"}'
# 202 Accepted  {"status":"queued"}
# 400 {"error":"Query cannot be empty"} | 500 {"error":"Internal queue full"}
```

### GET /api/trending

Snapshot of the top 10 queries by volume over the last 60 minutes, served from a
cached `Vec` refreshed on every batch flush.

```json
{"trending":[{"query":"weather today","score":20}]}
```

### GET /api/query

Full-text document search. Tokenizes `q`, fetches posting lists from
`inverted_index`, scores BM25-style IDF × TF, multiplies by `(1 + 10·PageRank)`,
returns matches sorted by combined score. `elapsed_ms` is the server-side
ranking time.

```bash
curl 'http://127.0.0.1:3000/api/query?q=rust%20concurrency'
```
```json
{"results":[{
  "url":"https://rust-lang.org/concurrency","title":"Rust Concurrency: Threads and Channels",
  "snippet":"... Concurrency in Rust is safe and performant ...",
  "score":12.84,"tf_idf_score":3.1,"pagerank_score":0.18
}],"elapsed_ms":0.717}
```

### POST /api/documents

Ingest a crawled page. Upserts the document and its outbound links in one
transaction, rebuilds its inverted-index postings, then recomputes PageRank over
the whole graph. `url`, `title`, `body` required; `links` is a list of URLs.

```bash
curl -X POST 'http://127.0.0.1:3000/api/documents' -H 'Content-Type: application/json' -d '{
  "url":"https://example.com/rust","title":"Rust Guide",
  "body":"Rust is a systems programming language ...","links":["https://rust-lang.org/intro"]}'
# 201 {"status":"indexed","url":"..."} | 400 on empty field
```

### GET /api/documents

All documents (`id,url,title,body,pagerank`) ordered by PageRank descending.

### GET /api/stats

```json
{"total_queries":5055,"total_indexed_documents":10,"active_trie_nodes":54617,
 "queue_size":0,"database_size_bytes":569344,"recent_logs":["[08:34] ..."]}
```

---

## Performance report

Measured on this machine: SQLite WAL, 5055 autocomplete queries (54,617 trie
nodes), 10 documents. Latency numbers are **end-to-end over loopback including
`curl` process spawn** — the handler itself is faster.

### Suggestion read latency — `GET /api/search`

300 requests across exact and fuzzy prefixes:

| avg | p50 | p95 | p99 | max |
|---|---|---|---|---|
| 0.68 ms | 0.62 ms | 1.12 ms | 1.40 ms | 1.53 ms |

Every read is a trie walk (`O(prefix length)`) plus, on a miss, a bounded
edit-distance DFS. No disk, no allocation on the common path. Suggestions stay
sub-millisecond at 5k queries and the cost is bounded by prefix length, not
corpus size.

### Document ranking latency — `GET /api/query` (server-side `elapsed_ms`)

| rust concurrency | react hooks | javascript async | sqlite wal | pagerank |
|---|---|---|---|---|
| 0.717 ms | 0.783 ms | 0.732 ms | 0.498 ms | 0.641 ms |

Each query opens its own SQLite connection on a `spawn_blocking` thread, so disk
work never blocks the async runtime.

### Cache hit rate

**Autocomplete reads hit SQLite 0% of the time** — the trie *is* the read
replica. It's built once at boot from the `queries` table and thereafter kept
warm by incremental in-place `apply()` from the batch worker, never re-read from
disk. By construction the suggestion hit rate is **100%**; there is no miss path
to fall back to. Trending serves the same way, from a cached snapshot. Only
`/api/query` and `/api/stats` read SQLite, and those are explicitly the
disk-backed paths.

The trade-off: the trie holds the full query set in RAM (54,617 nodes for 5k
queries) and a single-process restart is the only way to drop rows that were
deleted directly in SQLite.

### Write reduction through batching

The write path coalesces traffic in a `HashMap<query, count>` and flushes on a
2-second timer **or** once 50 events are buffered, whichever comes first.
Measured run — **600 rapid `POST /api/search` events across 12 unique queries**:

| metric | value |
|---|---|
| Raw write-intents (POST events) | 600 |
| DB transactions / commits (fsyncs) | ~12 |
| `queries`-table upserts per flush | 12 unique (not 600) |
| **Commit reduction** | **~50× fewer fsyncs** |

The expensive part of a SQLite write is the per-commit `fsync`; batching turns
~600 would-be commits into ~12. Repeated hits on the same query collapse into a
single `count = count + N` upsert instead of N separate writes. Honest caveat:
`query_logs` still gets **one row per event** (verified: +600 rows) because
trending needs per-event timestamps — batching reduces *commits and
`queries`-table churn*, not the raw `query_logs` row count. On flush failure the
buffer is retained and retried on the next tick, so a transient DB error never
loses counts.

---

## Design choices & trade-offs

Summarized here; full rationale with options-considered tables is in
[`decisions.md`](decisions.md).

- **Rust + Tokio + Axum.** The hot path is a per-keystroke in-memory read. No GC
  means no tail-latency spikes; `Arc<RwLock<…>>` gives cheap shared reads;
  `tokio::select!` drives the batch worker. Cost: slower to write than a scripted
  stack.
- **SQLite (WAL) as system of record.** Single-file, serverless, one concurrent
  writer + many readers — enough for a single node, with none of the operational
  weight of Postgres. Cost: doesn't scale past one box.
- **Trie with precomputed top-K per node.** Suggestions are `O(prefix length)`
  with zero post-processing because each node already holds its best 10. Cost:
  more memory and a wider write (every node on each suffix path updates its
  top-K). Chosen because reads vastly outnumber writes.
- **Suffix indexing for mid-word matches.** Each query is indexed under every
  word-boundary suffix, so "hooks" finds "react hooks guide". Cost: ~N× trie
  insertions per query.
- **Async batched writes off the hot path.** `POST` returns `202` instantly;
  durability is eventual (≤2 s, or on the 50-event threshold). Trade latency and
  a small crash window for throughput and a quiet disk.
- **Incremental trie `apply`, not rebuild.** Live deltas patch top-K lists in
  place — `O(query length)` per update — so a 5k-node index never rebuilds under
  traffic.
- **TF-IDF × PageRank, recomputed on ingest.** Power iteration (20 rounds,
  damping 0.85) runs synchronously inside `POST /api/documents`. Fine for a small
  graph; would move to a background job at scale.

---

## Layout

```
src/
  main.rs       boot, state wiring, router, batch-worker spawn
  handlers.rs   HTTP layer (the 7 endpoints)
  suggest.rs    trie SuggestEngine — prefix + suffix + Damerau-Levenshtein (unit tests here)
  cache.rs      Arc<RwLock<SuggestEngine>> wrapper: rebuild / apply / get
  batch.rs      background write worker — buffer, flush, retry
  trending.rs   cached trending snapshot
  db.rs         schema, WAL pragma, seed data, query helpers
  indexer.rs    tokenizer, inverted index, TF-IDF search_and_rank
  pagerank.rs   power-iteration PageRank with sink redistribution
  models.rs     serde wire types
static/         search UI (index.html/search.js) + telemetry console (console.html/app.js)
data/           typeahead.db (gitignored WAL/SHM sidecars)
```
