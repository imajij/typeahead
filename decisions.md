# Design Decisions

This document explains every significant system-design decision in the Googol
search simulator: what the problem was, which options were on the table, what
was chosen, and why. It covers the typeahead engine, the storage and write
path, the document-search side, the HTTP API, and the frontend.

The guiding constraint throughout: **this is a single-node, Google-style
search experience that must feel instant (suggestions in single-digit
milliseconds) while continuously learning from live traffic** — without the
operational weight of a distributed stack.

---

## 1. Runtime: Rust + Tokio + Axum

**Options considered**

| Option | Pros | Cons |
|---|---|---|
| Node.js / Express | Fast to write, JSON-native | GC pauses on a hot in-memory index; single-threaded CPU work blocks the event loop |
| Python / FastAPI | Quickest iteration | Too slow for per-keystroke trie traversal at scale; GIL fights the concurrent read path |
| Go | Great concurrency, simple deploys | Fine choice; weaker type system around shared mutable state |
| **Rust + Tokio + Axum** | No GC, fearless shared-state concurrency, `tokio::select!` for the batch worker, zero-cost reads on the hot path | Slower to write |

**Why Rust won.** The hot path here is a pure in-memory data-structure read
(trie walk) that happens on *every keystroke of every user*. That workload
wants: no garbage collector (no tail-latency spikes), cheap shared immutable
reads across threads (`Arc<RwLock<_>>`), and a type system that makes the
concurrent cache-update logic provably data-race-free. Axum was picked over
Actix/Warp for its minimal surface and first-class Tower middleware (CORS,
static file serving) — we use exactly three routes' worth of framework.

---

## 2. Storage: SQLite (WAL mode) as the system of record

**Options considered**

1. **PostgreSQL** — proper server DB. Overkill: adds a network hop, a second
   process to operate, and connection pooling for a demo-scale corpus.
2. **Redis** — great for counters, but it becomes *another* source of truth
   next to a relational store for documents/links, and persistence is an
   afterthought (RDB/AOF tuning).
3. **Flat files (CSV/JSON)** — no transactional batch writes, no ad-hoc
   queries for trending windows.
4. **SQLite, embedded, WAL mode** ← chosen.

**Why SQLite.** One file on disk, zero ops, real ACID transactions for batch
flushes, and SQL for the queries that genuinely benefit from it (the
trending time-window aggregation, the inverted-index joins).
`journal_mode=WAL` matters specifically because our access pattern is
**many concurrent readers + exactly one writer** (the batch worker): WAL lets
readers proceed while a write transaction commits, where the default rollback
journal would serialize them.

**Key consequence / accepted limitation:** rusqlite is synchronous, so every
DB touch from async handlers goes through `tokio::task::spawn_blocking`. We
open a connection per operation instead of pooling — at this request volume,
connection setup (~µs for an embedded DB) is noise, and it sidesteps pool
lifetime/poisoning complexity entirely.

**Division of labor (important):** SQLite is the *system of record*; it is
never on the suggestion read path. Autocomplete reads only ever touch the
in-memory engine. If the process dies, the trie is rebuilt from SQLite at
boot. This "durable store + disposable in-memory index" split is the same
shape Google-scale systems use (Bigtable + in-memory serving trees), shrunk
to one node.

---

## 3. The suggestion index: a trie with per-node top-K

This is the core decision of the project.

**The requirement.** Given a prefix, return the K most popular completions in
sub-millisecond time, on every keystroke.

**Options considered**

1. **SQL per keystroke** — `SELECT query FROM queries WHERE query LIKE 'p%'
   ORDER BY count DESC LIMIT 10`. Simple, always fresh. But it's a B-tree
   range scan + sort *per keystroke per user*, it hits the disk layer, and it
   can't express fuzzy or mid-word matching without table scans. Fine at toy
   scale; collapses exactly when a typeahead gets interesting.
2. **Sorted array of queries + binary search** for the prefix range, then a
   heap for top-K. Compact and cache-friendly, but top-K costs O(range) per
   lookup — a one-letter prefix like `r` scans a huge range. Precomputing
   top-K per *prefix range* effectively reinvents the trie's node lists.
3. **FST / DAWG (e.g. the `fst` crate)** — minimal memory, what Lucene/
   Elasticsearch use for completion suggesters. But FSTs are *immutable*:
   every live update would require a rebuild, which is precisely the property
   we're trying to escape (see §5). Right choice for huge static dictionaries;
   wrong one for a continuously learning index.
4. **Ternary search tree** — lighter per-node than a HashMap-child trie, but
   the same asymptotics with a more fiddly implementation; no help with our
   actual bottlenecks.
5. **Trie with top-K suggestion list materialized at every node** ← chosen.

**Why the trie wins here.** Lookup is O(L) in prefix length — *independent of
corpus size* — and because each node stores its subtree's top-K
`(query, count)` list, returning suggestions is literally "walk L nodes,
clone a ≤10-element vector". No sorting, no heap, no candidate collection at
query time. The price is memory: every node holds up to K references
(~1,700 nodes for the 40-query seed corpus). That trade — burn memory at
write time to make reads trivial — is the canonical typeahead trade-off, and
it's the right one because reads outnumber writes by orders of magnitude
(every keystroke reads; only a completed search writes).

**Per-node list size K=10** matches the API cap (Google shows 8; we default
to 8, clamp at 10). Storing more would be wasted memory; storing fewer would
make the API limit unreachable.

**Entry interning.** Queries are interned once into an `entries: Vec<Entry>`
registry and nodes store small `(id: u32, count)` refs instead of owned
strings. This keeps node lists 12 bytes/entry instead of a cloned `String`
per node per query (which would multiply each query string across every node
on its path), and — critically — makes deduplication by identity trivial
(see §4).

---

## 4. Mid-query word matching: word-boundary suffix indexing

**The requirement.** Typing `hooks` should suggest `react hooks guide` —
Google matches any word in the query, not just the first.

**Options considered**

1. **Prefix-only matching** (the original implementation) — misses most of
   what makes Google's typeahead feel smart.
2. **Separate inverted index** word → queries, merged with trie results at
   query time. Two data structures to keep consistent, and the merge step
   needs its own ranking pass; also can't serve *partial word* matches
   (`hoo` → `hooks`) without becoming a prefix structure itself anyway.
3. **Generalized suffix tree / suffix automaton** over all queries — matches
   *any infix* (`ook` → `hooks`). Substantially more complex, more memory,
   and infix matches below word granularity aren't actually useful for query
   suggestions (`ook` matching is noise, not signal).
4. **Insert every word-boundary suffix of each query into the same trie,
   all pointing at the same canonical entry id** ← chosen.

`react hooks guide` is inserted under three keys — `react hooks guide`,
`hooks guide`, `guide` — all referencing one interned entry.

**Why this one.** It reuses the *single* existing structure and its
precomputed top-K machinery wholesale: a mid-word match is just a normal
trie walk that happens to start at a suffix key. Word-boundary granularity
matches the actual user behavior (people start typing *words*). The cost is
bounded and predictable: a query with W words is indexed W times, so node
count scales by roughly the average word count (~3× here) — acceptable for
the read-speed win.

**The dedup subtlety this design forces:** suffix keys of the same query
share prefix-path nodes (`go go go` → keys `go go go`, `go go`, `go` all
walk through the `g→o` nodes). Because node lists are keyed by interned
entry **id**, the update is idempotent — "find id, update count, else push" —
so a query can never appear twice in one node's list. This is exactly why
interning (§3) isn't just a memory optimization; it's what makes suffix
indexing correct. (Covered by the `duplicate_words_do_not_duplicate_suggestions`
unit test.)

---

## 5. Live updates: incremental in-place path updates, not rebuilds

**The problem with the original design.** The first version rebuilt the
*entire* trie from a full `SELECT query, count FROM queries` on **every
2-second batch flush**. That is O(total corpus) work per flush, gets slower
forever as the corpus grows, holds the write lock for the whole rebuild, and
churns the allocator. It "worked" only because the corpus was 10 rows.

**Options considered**

1. **Full rebuild per flush** (status quo) — rejected for the above.
2. **Double-buffered rebuild** (build off to the side, atomic swap via
   `arc-swap`) — removes the lock-hold problem but keeps the O(corpus) CPU
   cost per flush. Right answer *if* updates had to be batched-immutable
   (e.g. an FST); wasteful when the structure supports point updates.
3. **Incremental in-place update along the insert path** ← chosen.

**The key insight that makes (3) correct:** a node's top-K list is the top-K
over all queries in its subtree, and every query in a node's subtree passes
through that node on its insert path. Therefore, updating a query's count
and re-walking *its own paths* — fixing up each node's list by "update entry
by id / insert / sort / truncate to K" — leaves every node list exactly as a
full rebuild would have. Initial load uses the *same* code path (insert each
query = walk its paths), so there's one update algorithm, not two.

**Cost:** O(L · W · K) per updated query (path length × word-suffix keys ×
list maintenance) — microseconds — versus O(corpus) for a rebuild.
A flush of a 50-query batch is ~50 such updates.

**Monotonicity makes truncation safe:** counts only ever increase. An entry
truncated out of a node's top-10 can only re-enter by growing past the
current 10th — which the "insert, sort, truncate" update naturally handles.
If decay/deletion were added later (see §14), this invariant breaks and
lists would need recomputation from children — a known, contained extension
point.

`Cache::rebuild` still exists, but only for boot-time loading.

---

## 6. Typo tolerance: bounded Damerau–Levenshtein DFS over the trie

**The requirement.** `raect` should still suggest react queries, flagged so
the UI can render them as corrections.

**Options considered**

1. **No fuzzy** — the single biggest gap between "demo autocomplete" and
   "feels like Google".
2. **SymSpell** (precomputed delete-variants) — the fastest known approach
   for spell-check at scale, but it precomputes a dictionary of all
   single/double-deletes of all *terms*: a large second index, rebuilt as the
   corpus learns, and it corrects whole words rather than *prefixes* (our
   queries are open-ended prefixes, not complete words).
3. **BK-tree** over queries — classic metric-tree approach; needs a separate
   structure, and again compares *complete strings*, not prefixes.
4. **Levenshtein automata** intersected with the trie (the Lucene approach) —
   the asymptotically right answer, but building the automaton per query is
   significant implementation complexity for a corpus this size.
5. **DFS over the existing trie carrying a Damerau–Levenshtein DP row per
   node** ← chosen.

**Why (5).** It needs *zero additional index*: the same trie (including its
suffix keys and per-node top-K lists) is explored with a textbook dynamic-
programming row per visited node. Pruning is natural — descend only while
`min(row) ≤ max_edits`, which is guaranteed to terminate because the row
minimum is bounded below by the length difference. Any node whose path
string is within budget of the typed prefix contributes its *precomputed*
top-K (so even fuzzy results pay no collection cost). The candidate set is
deduped by entry id keeping the best `(distance, count)`.

**Why Damerau (transpositions count as one edit) rather than plain
Levenshtein:** adjacent transposition is the single most common typing error
(`raect`, `teh`). Under plain Levenshtein a transposition costs 2 edits and
would blow the budget for short prefixes — `raect` would *not* match `react`
with 1 edit. The restricted-Damerau extension is ~6 lines of extra DP state
(previous row + previous character).

**Edit budget scales with prefix length** — 0 edits for ≤2 chars, 1 for 3–5,
2 for 6+ — mirroring how real engines behave: short prefixes have too little
signal to correct ("zz" should return nothing, not wild guesses), long ones
can absorb two errors. Fuzzy candidates rank strictly **after** exact
matches, ordered by edit distance then popularity, and carry a `fuzzy: true`
flag so the UI can render them differently (amber/italic, `~fixed` tag) and
drive "did you mean".

---

## 7. Ranking: popularity counts, exact-before-fuzzy

**Options considered:** raw frequency; frequency with time-decay; learned
ranking (CTR); personalization.

**Chosen:** raw frequency for exact matches (ties broken deterministically by
insertion id), `(edit distance, frequency)` for fuzzy. Personalization is
done **client-side** (§13) and *recency* is served by a separate channel —
the trending system (§9) — rather than baked into the suggestion score.

**Why.** Frequency is the dominant signal in real query suggestion and is
the only one we can compute honestly from the data we have. Time-decayed
counts would couple the trie's update rule to wall-clock time and break the
monotonicity that makes incremental updates safe (§5) — a real cost for a
speculative gain. Keeping "all-time popular" (trie) and "popular right now"
(trending cache) as two separate, simple systems is both easier to reason
about and closer to how production systems actually decompose the problem.

---

## 8. Write path: bounded async queue + batching worker

**The requirement.** Recording a search must never slow down the user, and
neither the DB nor the trie should be hit once per raw event under load.

**Options considered**

1. **Synchronous write per search** — couples user-facing latency to disk;
   one SQLite writer becomes the bottleneck under bursts.
2. **Fire-and-forget spawn per event** — unbounded task pile-up under load;
   no batching, so the DB still sees per-event transactions.
3. **External queue (Kafka/Redis Streams)** — the "real" architecture at
   scale, absurd for one node.
4. **Bounded `tokio::mpsc` channel (capacity 2000) into a single batch
   worker** ← chosen.

The handler does `tx.send(query)` and immediately returns **202 Accepted** —
the semantically honest status for "queued, not yet durable". The worker
accumulates per-query deltas in a `HashMap` and flushes when **either** 2
seconds elapse (`tokio::interval`, so a steady trickle still flushes on
time) **or** 50 buffered events accumulate (bounds memory and staleness
under bursts). Each flush is **one SQLite transaction** (one fsync amortized
over the batch) using `INSERT ... ON CONFLICT ... count = count + ?` upserts,
then the *same deltas* are applied incrementally to the trie (§5) and the
trending cache is refreshed.

**Backpressure & failure semantics, explicitly chosen:**
- The channel is *bounded*: if the worker ever falls behind, producers get an
  error and the API reports it — load-shedding rather than OOM.
- On a failed flush, the delta buffer is **retained** and retried on the next
  tick (writes are idempotent upserts, so retry is safe).
- Buffered-but-unflushed events are **lost on crash**. Accepted deliberately:
  query-popularity counts are statistical data where a ≤2s/50-event loss
  window is irrelevant; paying an fsync per keystroke-event to avoid it
  would be the wrong trade. (A WAL-style append log would close the gap if
  the counts ever became billing-grade data.)
- Counter consistency: the trie is updated *only* through the same deltas
  that were committed to SQLite, and the boot path reloads from SQLite — so
  the in-memory counts can never silently diverge from the durable ones.

---

## 9. Trending: windowed log table + in-memory snapshot

**The requirement.** "Trending now" = popular *recently*, not all-time — a
different question than the trie answers.

**Options considered:** exponentially-decayed counters folded into the main
counts (couples two concerns, breaks §5 monotonicity); count-min sketch +
ring buffer (right at huge scale, needless approximation here); **an
append-only `query_logs(query, timestamp)` table aggregated with
`GROUP BY query` over a 60-minute window** ← chosen.

**Why.** It's exact, it's one indexed SQL query (`idx_query_logs_timestamp`),
and the log table doubles as an audit trail. The aggregation runs only once
per batch flush (not per HTTP request) and the result is cached in a
`TrendingCache` (`Arc<RwLock<Vec<_>>>`), so `GET /api/trending` is a pure
memory read. The cache being ≤2s + flush-interval stale is invisible for a
"trending" widget. The zero-state dropdown and the "I'm Feeling Lucky"
button reuse this same endpoint, with a 30s client-side cache.

---

## 10. Concurrency model for the shared index

**Options considered:** `Mutex` (serializes readers — wrong for a
read-dominated cache); sharded locks (complexity without need at this
scale); `arc-swap` double-buffering (pairs with rebuilds, which §5
eliminated); **`Arc<RwLock<SuggestEngine>>` with Tokio's RwLock** ← chosen.

**Why.** Reads (every keystroke) take a shared lock and do O(L) work plus a
≤10-element clone — concurrent readers don't block each other. The only
writer is the batch worker, whose incremental updates (§5) hold the
exclusive lock for microseconds per flush, not the milliseconds-to-seconds a
full rebuild held it. Choosing the *async* RwLock keeps handlers honest
(awaiting instead of blocking a runtime thread), even though hold times are
tiny.

The engine itself contains **no** interior synchronization — it's a plain
single-threaded data structure wrapped at the boundary. That keeps the
algorithmic code testable (plain unit tests, no async) and the locking
strategy swappable.

---

## 11. Document search: SQLite inverted index + TF-IDF × PageRank

The "results page" half of the simulator, deliberately kept educational and
inspectable rather than maximally clever:

- **Inverted index as a table** `inverted_index(term, doc_id, tf)` instead of
  SQLite FTS5 or tantivy: the point of this project is to *show* the
  postings/IDF/ranking mechanics; an off-the-shelf FTS module would hide
  exactly the part worth demonstrating. The `term` index makes posting-list
  fetches a single indexed `IN` query.
- **Scoring:** TF-IDF with the BM25 IDF form `ln((N − df + 0.5)/(df + 0.5))`
  (better small-corpus behavior than naive `ln(N/df)`, which zeroes out
  terms that appear in every doc), combined with link authority as
  `score = tfidf × (1 + 10 × pagerank)`. Multiplicative blending was chosen
  over additive because the two scores live on incommensurable scales;
  the `10×` factor makes authority a meaningful multiplier on a normalized
  (sums-to-1) PageRank without letting it override topical relevance.
- **PageRank by power iteration** (damping 0.85, 20 fixed iterations, sink-
  mass redistribution, normalized) recomputed **synchronously on every
  document ingest**. At tens-to-hundreds of documents an exact recompute is
  milliseconds; incremental/approximate PageRank is real-engine territory
  with none of the payoff here. Ingest is rare and admin-driven, so it can
  afford to be the slow path.
- Queries against documents run in `spawn_blocking` and report `elapsed_ms`,
  which the UI surfaces as the classic "N hits · 0.003s" line.

A revamp fix worth recording: document ingest *used to* rebuild the
suggestion trie, though ingesting a document doesn't touch the queries table
at all — a pure waste coupling the two subsystems. Removed.

---

## 12. API design

- **`GET /api/search?q=&limit=` (suggest) vs `POST /api/search` (record) vs
  `GET /api/query` (document search)** — three distinct operations with
  distinct semantics: an idempotent read, a side-effecting event ingest, and
  a heavier read. Overloading one endpoint would make caching and reasoning
  worse; the GET/POST split on `/api/search` keeps the resource naming
  intuitive ("searches").
- **Only *executed* searches are recorded**, never raw keystrokes/prefixes.
  This is both signal hygiene (prefixes are noise — recording them would
  teach the engine `r`, `ru`, `rus`…) and the user-expectation-respecting
  choice.
- **Normalization happens server-side at both boundaries**
  (`normalize_query`: lowercase, whitespace-collapse, 100-char cap — applied
  to suggestion lookups *and* recorded queries). Doing it in one shared
  function guarantees the trie's keys and lookups can never drift apart;
  trusting the client to normalize would make cache keys attacker/typo
  -controlled.
- **Suggestions carry `(query, count, fuzzy)`** — `count` lets the UI show
  popularity, `fuzzy` lets it distinguish corrections (amber italic +
  "did you mean"). The server does not return highlight ranges: the
  bold-the-untyped-part presentation is a *pure* function of
  (typed text, suggestion), so computing it client-side avoids paying
  per-suggestion markup bytes on every keystroke.
- **`limit` is clamped server-side (1..=10)** — never trust the client with
  an unbounded fan-out parameter.

---

## 13. Frontend architecture

- **Vanilla JS/CSS/HTML, zero build step.** A framework buys component
  state management we don't need (one input, one list, one results pane) and
  costs a toolchain. The entire client is ~450 lines served statically by
  `ServeDir`. This also keeps the demo's full request path inspectable.
- **Race-proofing the typeahead** (the actually-hard part of autocomplete
  UIs): 110ms debounce (below perception, above per-keystroke spam);
  `AbortController` cancels in-flight suggestion fetches when a newer one
  starts; *and* a stale-response guard compares the response's query against
  the current input before rendering — belt-and-suspenders because abort
  alone can't catch a response already in the JS task queue. The same guard
  pattern protects result rendering and did-you-mean.
- **Search history lives in `localStorage`, not on the server.** Per-user
  personalization without accounts, sessions, or privacy surface; history
  entries render in violet with per-item Remove, merged ahead of server
  suggestions and deduped against them. The server stays user-agnostic.
- **Google's exact keyboard semantics**, because muscle memory is the spec:
  ↓/↑ move selection *and fill the input*, wrapping through a "nothing
  selected = your typed text" state; Esc restores the typed text; Enter
  searches the filled value; `/` focuses the box from anywhere.
- **URL as state** (`?q=` + `pushState`/`popstate`): results pages are
  linkable, back/forward work, and a reload lands on the same SERP — no
  router library needed for a two-view app.
- **"Did you mean" is composed client-side** from the existing suggest API
  (fetch suggestions for the full query; if the best is `fuzzy`, offer it).
  No dedicated endpoint: the suggest engine already *is* the spell-checker,
  so reusing it keeps one source of correction truth.
- **Theming**: terminal-glassmorphism (IBM Plex Mono everywhere, phosphor
  green/cyan/amber on near-black, frosted `backdrop-filter` panels over
  drifting gradient orbs, scanline overlay). Dark-only by design — declared
  via `<meta name="color-scheme" content="dark">` rather than maintaining a
  light variant nobody asked for. Motion respects
  `prefers-reduced-motion`.
- A global `[hidden] { display: none !important }` exists because any
  author `display:` on an element silently defeats the HTML `hidden`
  attribute — a bug class this codebase hit in practice (the clear button
  rendering on an empty search box).

---

## 14. Known limits and the upgrade path

Decisions above are right *for this scale*; here is what consciously breaks
first, and what the next step would be:

| Pressure | First crack | Next step |
|---|---|---|
| Corpus → millions of queries | Trie memory (HashMap children, per-node vectors) | Array-mapped/compressed children; or FST for a static head + small mutable delta trie, merged periodically |
| Multi-node deployment | In-process trie + SQLite file | Counts to Redis/Postgres; suggestion service becomes a replicated read layer rebuilt from snapshots |
| Count decay / query deletion | §5's monotonicity invariant | Periodic offline rebuild (the `rebuild` path already exists) or subtree top-K recomputation on decrement |
| Abuse / junk queries | Anything typed twice becomes a suggestion | Frequency + distinct-user thresholds before a query becomes suggestible; denylist at `normalize_query` |
| Durability of counts | ≤2s/50-event loss window (§8) | Append-only event log fsynced ahead of ack, replayed at boot |
| Crash before flush vs. trending | `query_logs` loses the same window | Same event-log fix covers both |

The unit-test suite (`src/suggest.rs`) pins the engine's behavioral
contract: prefix ranking, mid-word matching, substitution *and*
transposition correction, no-fuzzy-on-short-prefixes, in-place re-ranking
after increments, and suffix-key dedup — the properties the above decisions
promise.
