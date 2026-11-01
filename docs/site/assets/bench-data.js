/* Every benchmark number on the showcase site, in one place.
 *
 * Sections follow the laptop benchmark plan: A1 replacing SQLite, A2 full
 * text search, A3 recommender (Part A, one file in an app), B a 2-shard
 * cluster on kind on the laptop (Part B), and C start small, go big (Part C).
 *
 * A cell is
 *   a number                      a value, in the chart's unit (or the row's)
 *   null                          a placeholder: drawn hatched, with no value
 *   {v, p90, p99, s, note}        a value with details; s overrides the status
 *   "na:why"                      the system cannot run it; `why` is shown
 *
 * A cell's status is its own `s`, else its column's `status`, else the
 * chart's `status`:
 *   "measured"     from the full laptop benchmark (run "laptop-2026-11")
 *   "earlier"      measured in an earlier run, named by `run`; replaced when
 *                  the benchmark re-runs it
 *   "placeholder"  nothing measured yet
 *
 * To fill a chart from the benchmark: set its `run` to "laptop-2026-11",
 * its `status` to "measured", and replace the numbers. Numbers here are
 * measured, never edited by hand (AGENTS.md), and each run names its machine.
 */
window.STRATA_BENCH = {
  runs: {
    "laptop-2026-11": {
      label: "Laptop benchmark, November 2026",
      machine: "MacBook Air, Apple M2 (4 performance + 4 efficiency cores), 24 GB, macOS 26.0.1",
      source: "strata/results/laptop-2026-11/ and strata-server/results/laptop-2026-11/"
    },
    "m2-local-2026-10": {
      label: "Earlier run, October 2026",
      machine: "MacBook Air, Apple M2, 24 GB, macOS 26.0.1; one query thread; warm cache; one segment",
      source: "docs/strata/results.md (strata/results/strata/, strata/results/baselines/)"
    },
    "m2-kind-2026-10-28": {
      label: "Earlier run, 28 October 2026",
      machine: "The same MacBook Air: kind 0.33 in Docker Desktop (4 CPUs, 8.7 GB), 1 control plane and 3 workers; 3 CPUs and 6 GiB per engine",
      source: "strata-server/results/compare-mac/"
    },
    "ci-gate-2026-10": {
      label: "Earlier run, October 2026",
      machine: "Cloud VM (CI's recommender gate)",
      source: "strata/results/sequence/gate.py"
    }
  },

  headlines: [
    { value: "4.7×", text: "lower median BM25 latency than Elasticsearch, one client, two shards each on Kubernetes", status: "earlier", run: "m2-kind-2026-10-28" },
    { value: "2.6×", text: "Elasticsearch's BM25 throughput at 16 clients, with the same 3 CPUs and 6 GiB", status: "earlier", run: "m2-kind-2026-10-28" },
    { value: "1.7–3.2×", text: "faster warm BM25 than Tantivy, from one file, on four BEIR collections", status: "earlier", run: "m2-local-2026-10" },
    { value: "0.05–0.10 ms", text: "for a vector top 10 with HNSW at recall@10 0.99, 1,024 dimensions", status: "earlier", run: "m2-local-2026-10" },
    { value: null, text: "An app's database on the shop data: Strata against SQLite 3.53 and DuckDB", status: "placeholder" },
    { value: null, text: "The same cluster test against OpenSearch 3 and Vespa 8", status: "placeholder" }
  ],

  suites: [
    /* ------------------------------------------------------------ A1 */
    {
      id: "a1", part: "A", title: "Replacing SQLite: an app's database in one file",
      lede: "The shop app's own database: products, reviews and users, loaded, read, written, paged, joined and counted the way an app does it, from Python and, for the sub-millisecond kinds, from Rust.",
      dataset: "Shop: Amazon Reviews 2023, Video Games (137K products, 4.6M reviews, 2.8M users)",
      plan: [
        "SQLite 3.53 in WAL mode at synchronous=FULL and NORMAL, and DuckDB 1.x, each driven the way an app uses it",
        "Load, open and first query, get by key, list pages, aggregates, joins, a mixed 1-writer 4-reader load, footprint",
        "Writes, updates and a stream of events, against SQLite, DuckDB and LanceDB: rows a second, p50 and p99 per write, and the time until a write is visible to a query; every write is read back",
        "Where Strata loses (DuckDB on scans and large aggregates, SQLite perhaps on single-row durable writes), the table says so"
      ],
      charts: [
        {
          id: "a1-reads", title: "Reads", sub: "Warm p50 in ms, lower is better", unit: "ms", better: "lower", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", self: true }, { name: "SQLite", ver: "3.53, WAL" }, { name: "DuckDB", ver: "1.x" }],
          rows: [
            { label: "open the file and answer a query", cells: [null, null, null] },
            { label: "a product by id", unit: "µs", cells: [null, null, null] },
            { label: "a user's reviews by user id", unit: "µs", cells: [null, null, null] },
            { label: "list page: category, price range, by rating, first 20", cells: [null, null, null] },
            { label: "the same list page at offset 10,000", cells: [null, null, null] },
            { label: "average rating and count per brand", cells: [null, null, null] },
            { label: "reviews per month", cells: [null, null, null] },
            { label: "top products with their review counts (join)", cells: [null, null, null] },
            { label: "a user's reviewed products (join)", cells: [null, null, null] }
          ]
        },
        {
          id: "a1-writes", title: "Inserts", sub: "Rows a second, higher is better", unit: "rows/s", better: "higher", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", self: true }, { name: "SQLite", ver: "synchronous=FULL" }, { name: "SQLite", ver: "synchronous=NORMAL" }, { name: "DuckDB", ver: "1.x" }, { name: "LanceDB" }],
          rows: [
            { label: "load products and reviews from JSONL", cells: [null, null, null, null, null] },
            { label: "1 row per durable commit", cells: [null, null, null, null, null] },
            { label: "100 rows per transaction", cells: [null, null, null, null, null] },
            { label: "1,000 rows per transaction", cells: [null, null, null, null, null] },
            { label: "10,000 rows per transaction", cells: [null, null, null, null, null] },
            { label: "p50 per durable insert", unit: "ms", better: "lower", cells: [null, null, null, null, null] },
            { label: "p99 per durable insert", unit: "ms", better: "lower", cells: [null, null, null, null, null] },
            { label: "mixed: 1 writer, 4 readers, 80% reads", unit: "q/s", cells: [null, null, null, null, null] }
          ],
          note: "Into a table with its text and vector indexes, so every system indexes what it writes."
        },
        {
          id: "a1-updates", title: "Updates and deletes", sub: "Rows a second, higher is better; latency rows lower is better", unit: "rows/s", better: "higher", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", self: true }, { name: "SQLite", ver: "3.53, WAL" }, { name: "DuckDB", ver: "1.x" }, { name: "LanceDB" }],
          rows: [
            { label: "a price or stock by id", cells: [null, null, null, null] },
            { label: "a counter + 1", cells: [null, null, null, null] },
            { label: "a text field changed (re-indexed)", cells: [null, null, null, null] },
            { label: "a vector replaced", cells: [null, null, null, null] },
            { label: "upsert", cells: [null, null, null, null] },
            { label: "delete by id", cells: [null, null, null, null] },
            { label: "p50 per durable update", unit: "ms", better: "lower", cells: [null, null, null, null] },
            { label: "p99 per durable update", unit: "ms", better: "lower", cells: [null, null, null, null] },
            { label: "time until an update is visible", unit: "ms", better: "lower", cells: [null, null, null, null] }
          ]
        },
        {
          id: "a1-stream", title: "A stream of events", sub: "Review events appended while 4 readers query", unit: "ms", better: "lower", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", self: true }, { name: "SQLite", ver: "3.53, WAL" }, { name: "DuckDB", ver: "1.x" }, { name: "LanceDB" }],
          rows: [
            { label: "events a second, flat out", unit: "ev/s", better: "higher", cells: [null, null, null, null] },
            { label: "time until visible at 10K events a second", cells: [null, null, null, null] },
            { label: "read p99 at 1K events a second", cells: [null, null, null, null] },
            { label: "read p99 at 10K events a second", cells: [null, null, null, null] },
            { label: "read p99 at 50K events a second", cells: [null, null, null, null] }
          ]
        },
        {
          id: "a1-footprint", title: "Footprint", sub: "MB, lower is better", unit: "MB", better: "lower", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", self: true }, { name: "SQLite", ver: "3.53, WAL" }, { name: "DuckDB", ver: "1.x" }],
          rows: [
            { label: "file after the load", cells: [null, null, null] },
            { label: "file after 10% of rows updated", cells: [null, null, null] },
            { label: "memory while serving the reads", cells: [null, null, null] }
          ],
          note: "Strata's file also holds the full-text, vector and graph indexes the app's search and recommendations use; SQLite's and DuckDB's do not."
        }
      ]
    },

    /* ------------------------------------------------------------ A2 */
    {
      id: "a2", part: "A", title: "Full-text search: a local search box",
      lede: "BM25, filters, facets, typos, prefixes, vectors and hybrid ranking from one embedded file, against the libraries an app would embed instead.",
      dataset: "BEIR SciFact, FiQA, TREC-COVID and Quora (5K to 523K documents, judged), and the shop data for facets, typos and prefixes",
      plan: [
        "SQLite FTS5, Tantivy 0.26, DuckDB's fts extension and LanceDB",
        "BM25; AND and OR; phrase; BM25 with a filter; facets; prefix autocomplete; typo-tolerant; vector; vector with a filter; hybrid RRF and weighted",
        "Build time, index size, p50 and p99 warm and cold at 1 and 4 threads, nDCG@10 and recall@10. A kind a system lacks is \"not supported\"; one an app would write itself is run that way and labelled \"app code\""
      ],
      charts: [
        {
          id: "a2-bm25", title: "BM25 top 10", sub: "Warm median in ms, log scale, lower is better", unit: "ms", better: "lower", scale: "log", status: "earlier", run: "m2-local-2026-10",
          columns: [{ name: "Strata", self: true }, { name: "Tantivy", ver: "0.26" }, { name: "SQLite FTS5" }, { name: "DuckDB fts", status: "placeholder" }, { name: "LanceDB", status: "placeholder" }],
          rows: [
            { label: "SciFact (5K)", cells: [0.048, 0.153, 2.06, null, null] },
            { label: "FiQA (58K)", cells: [0.158, 0.368, 12.0, null, null] },
            { label: "TREC-COVID (171K)", cells: [0.391, 0.681, 78.3, null, null] },
            { label: "Quora (523K)", cells: [0.098, 0.193, 9.72, null, null] }
          ],
          note: "FTS5 scores every match; Strata and Tantivy skip what cannot reach the top 10."
        },
        {
          id: "a2-vector", title: "Vector top 10", sub: "Warm median in ms, log scale, lower is better", unit: "ms", better: "lower", scale: "log", status: "earlier", run: "m2-local-2026-10",
          columns: [{ name: "Strata", ver: "HNSW", self: true }, { name: "SQLite", ver: "scan of blobs" }, { name: "Tantivy" }, { name: "DuckDB vss", status: "placeholder" }, { name: "LanceDB", status: "placeholder" }],
          rows: [
            { label: "SciFact", cells: [0.046, 7.66, "na:no vectors", null, null] },
            { label: "FiQA", cells: [0.072, 82.3, "na:no vectors", null, null] },
            { label: "TREC-COVID", cells: [0.066, 265, "na:no vectors", null, null] },
            { label: "Quora", cells: [0.097, 695, "na:no vectors", null, null] }
          ],
          note: "SQLite had no vector index in that run, so its numbers are a scan; the laptop benchmark compares ANN indexes."
        },
        {
          id: "a2-hybrid", title: "Hybrid: BM25 and vectors fused by RRF", sub: "Warm median in ms, log scale, lower is better", unit: "ms", better: "lower", scale: "log", status: "earlier", run: "m2-local-2026-10",
          columns: [{ name: "Strata", self: true }, { name: "SQLite", ver: "FTS5 + scan" }, { name: "Tantivy" }, { name: "DuckDB", status: "placeholder" }, { name: "LanceDB", status: "placeholder" }],
          rows: [
            { label: "SciFact", cells: [0.170, 10.1, "na:no vectors", null, null] },
            { label: "FiQA", cells: [0.418, 95.0, "na:no vectors", null, null] },
            { label: "TREC-COVID", cells: [0.772, 344, "na:no vectors", null, null] },
            { label: "Quora", cells: [0.343, 714, "na:no vectors", null, null] }
          ]
        },
        {
          id: "a2-quality", title: "The same answers", sub: "BM25 nDCG@10 against BEIR's judgments, higher is better", unit: "", digits: 3, better: "higher", scale: "unit", status: "earlier", run: "m2-local-2026-10",
          columns: [{ name: "Strata", self: true }, { name: "Tantivy", ver: "0.26" }, { name: "SQLite FTS5" }, { name: "DuckDB fts", status: "placeholder" }, { name: "LanceDB", status: "placeholder" }],
          rows: [
            { label: "SciFact", cells: [0.689, 0.687, 0.684, null, null] },
            { label: "FiQA", cells: [0.252, 0.251, 0.254, null, null] },
            { label: "TREC-COVID", cells: [0.658, 0.667, 0.641, null, null] },
            { label: "Quora", cells: [0.756, 0.758, 0.760, null, null] }
          ],
          note: "Within 0.018 of each other everywhere: speed is not bought with worse answers."
        },
        {
          id: "a2-shop", title: "The shop's search box", sub: "Warm p50 in ms on the shop data, lower is better", unit: "ms", better: "lower", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", self: true }, { name: "SQLite FTS5" }, { name: "Tantivy", ver: "0.26" }, { name: "DuckDB fts" }, { name: "LanceDB" }],
          rows: [
            { label: "BM25 with a price filter", cells: [null, null, null, null, null] },
            { label: "phrase", cells: [null, null, null, null, null] },
            { label: "facets over the matches", cells: [null, null, null, null, null] },
            { label: "prefix autocomplete", cells: [null, null, null, null, null] },
            { label: "typo-tolerant", cells: [null, null, null, null, null] },
            { label: "vector with a filter", cells: [null, null, "na:no vectors", null, null] },
            { label: "hybrid, weighted", cells: [null, null, "na:no vectors", null, null] }
          ]
        },
        {
          id: "a-operators", type: "ops", title: "Operator by operator, in one file", sub: "What each library can run, and its p50 in ms where it can", unit: "ms", status: "placeholder",
          columns: [{ name: "Strata", self: true }, { name: "SQLite", ver: "FTS5" }, { name: "Tantivy", ver: "0.26" }, { name: "DuckDB", ver: "fts" }, { name: "LanceDB" }],
          rows: [
            { group: "Words" },
            { label: "AND, OR, NOT, nested", cells: ["y", "y", "y", "c", "c"] },
            { label: "word weights", cells: ["y", "c", "y", "c", "c"] },
            { label: "at least n of the words", cells: ["y", "c", "c", "c", "c"] },
            { label: "phrase", cells: ["y", "y", "y", "n:no phrase queries in its fts extension", "c"] },
            { label: "words near each other", cells: ["n:not yet", "y:NEAR", "y:phrase slop", "n", "c"] },
            { label: "prefix of a word", cells: ["y", "y:prefix*", "y", "n:no prefix queries in its fts extension", "c"] },
            { label: "typos", cells: ["y", "n:no fuzzy matching", "y:fuzzy term query", "n", "c"] },
            { label: "query-time synonyms", cells: ["y", "n", "c", "c", "c"] },
            { label: "highlighted snippets", cells: ["n:not yet", "y:snippet(), highlight()", "y:snippet generator", "c", "c"] },
            { group: "Ranking" },
            { label: "hybrid RRF", cells: ["y", "p:in app code", "n:no vectors", "p:in SQL by hand", "c"] },
            { label: "normalised weighted fusion", cells: ["y", "p:in app code", "n:no vectors", "p:in SQL by hand", "c"] },
            { label: "boosts by a column or a decay", cells: ["y", "p:in SQL by hand", "c", "p:in SQL by hand", "c"] },
            { label: "at most k rows per value", cells: ["y", "p:window function", "c", "p:window function", "c"] },
            { group: "SQL" },
            { label: "JSON paths of keys no schema names", data: "shop", cells: ["y", "y:json_extract, not indexed for search", "c", "y", "c"] },
            { label: "GROUP BY two fields with HAVING", cells: ["y", "y", "n:no SQL", "y", "c"] },
            { label: "joins of two tables", data: "shop", cells: ["y", "y", "n:no SQL", "y", "c"] },
            { label: "correlated subqueries", data: "shop", cells: ["y", "y", "n:no SQL", "y", "c"] },
            { label: "window functions, UNION", cells: ["n:until DataFusion's next step", "y", "n:no SQL", "y", "c"] },
            { label: "graph walks over an edge table", data: "shop", cells: ["y", "p:recursive CTE", "n", "p:recursive CTE", "c"] },
            { label: "percentiles", cells: ["n:not yet", "c", "c", "y", "c"] }
          ],
          note: "Marks come from each library's documentation and are confirmed or corrected by the runs. Strata's own gaps stay in the table."
        }
      ]
    },

    /* ------------------------------------------------------------ A3 */
    {
      id: "a3", part: "A", title: "Recommendations in the same file",
      lede: "Similar items, the next item for a user and a filtered home feed, each one SQL query against the shop's file, against the two-part stacks apps build today.",
      dataset: "Shop data for latency; Amazon 2014 Beauty, Sports and Toys (5-core) for quality against published TIGER and SASRec",
      plan: [
        "Similar items with stock, price and already-bought filters: Strata against LanceDB, sqlite-vec, and FAISS HNSW plus SQLite",
        "Next item for a user: encode() plus nearest neighbours, a walk over co-review edges, and their RRF fusion",
        "Home feed: two candidate sources plus rules (drop seen, at most 2 per brand, a price filter) in one query, against the same logic in app code",
        "Recall@10 and NDCG@10 over all test users, p50 and p99 per request, training or build time, file size"
      ],
      charts: [
        {
          id: "a3-ml1m", title: "Next item: Strata's sources on MovieLens-1M", sub: "Recall@10, higher is better", unit: "", digits: 3, better: "higher", scale: "unit", status: "earlier", run: "ci-gate-2026-10",
          columns: [{ name: "Recall@10" }],
          rows: [
            { label: "encode(): a sequence model, trained an hour", self: true, cells: [0.306] },
            { label: "ppr over what came after what", self: true, cells: [0.242] },
            { label: "counted next items", self: true, cells: [0.211] },
            { label: "item-kNN (baseline)", cells: [0.069] }
          ],
          note: "CI's recommender gate checks these on every change; the laptop benchmark adds published baselines on the Amazon sets."
        },
        {
          id: "a3-amazon", title: "Next item against published models", sub: "Recall@10 on Amazon 2014, leave-one-out, higher is better", unit: "", digits: 4, better: "higher", scale: "row", status: "placeholder",
          columns: [{ name: "Strata encode()", self: true }, { name: "Strata graph walk", self: true }, { name: "Strata RRF of both", self: true }, { name: "SASRec", ver: "published" }, { name: "TIGER", ver: "published" }],
          rows: [
            { label: "Beauty", cells: [null, null, null, null, null] },
            { label: "Sports", cells: [null, null, null, null, null] },
            { label: "Toys", cells: [null, null, null, null, null] }
          ]
        },
        {
          id: "a3-latency", title: "A request's time", sub: "p50 in ms on the shop data, lower is better", unit: "ms", better: "lower", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", self: true }, { name: "LanceDB" }, { name: "sqlite-vec" }, { name: "FAISS + SQLite" }],
          rows: [
            { label: "similar items, filtered", cells: [null, null, null, null] },
            { label: "next item for a user", cells: [null, null, null, null] },
            { label: "home feed: two sources and rules", cells: [null, null, null, null] }
          ]
        }
      ]
    },

    /* ------------------------------------------------------------- B */
    {
      id: "b", part: "B", title: "A cluster on Kubernetes, on the laptop",
      lede: "Two shards per engine on kind, with the same CPUs and memory for each, one engine at a time. The 28 October run measured Strata, Elasticsearch and Solr on Quora; the laptop benchmark adds OpenSearch and Vespa, MS MARCO's 2M passages, five more kinds, writes, updates and streams, 27 operators one by one, and operations tests.",
      dataset: "B1: BEIR Quora (523K questions, 1,024-dimension vectors, judged). B2: MS MARCO v2.1, 2M passages, 768-dimension vectors, larger than the memory budget",
      plan: [
        "Elasticsearch 9 (Basic licence, its default), OpenSearch 3, Solr 10 and Vespa 8, each with 2 data nodes in 3 CPUs and 6 GiB; Strata as 2 shards and a coordinator",
        "14 kinds at 1, 4 and 16 clients: the nine of 28 October plus phrase, typo-tolerant, prefix autocomplete, similar items with exclusions and a SQL aggregate",
        "Writes, partial updates and a Kafka-compatible stream (Redpanda, 4 partitions) under load: documents a second, p50 and p99, time until visible, and what a consumer restart loses or repeats",
        "27 operators, each timed wherever an engine has it and marked where it has none; Elasticsearch's paid hybrid retrievers run on a trial licence, labelled as such",
        "Load, live scale-out from 2 to 3 shards, failover with one replica per shard, footprint"
      ],
      charts: [
        {
          id: "b-p50", title: "One client", sub: "Median latency in ms, lower is better; hover for p90 and p99", unit: "ms", better: "lower", scale: "row", status: "earlier", run: "m2-kind-2026-10-28",
          columns: [{ name: "Strata", ver: "2 shards + coordinator", self: true }, { name: "Elasticsearch", ver: "9.5.5" }, { name: "Elasticsearch", ver: "trial licence", status: "placeholder" }, { name: "OpenSearch", ver: "3", status: "placeholder" }, { name: "Solr", ver: "10.1" }, { name: "Vespa", ver: "8", status: "placeholder" }],
          rows: [
            { label: "BM25", cells: [{ v: 0.68, p90: 0.89, p99: 1.8 }, { v: 3.20, p90: 5.56, p99: 12.8 }, "skip:only the hybrid kinds, which need the paid features", null, { v: 3.08, p90: 5.52, p99: 18.4 }, null] },
            { label: "BM25 + filter", cells: [{ v: 0.87, p90: 1.12, p99: 1.7 }, { v: 1.57, p90: 2.54, p99: 7.2 }, "skip:only the hybrid kinds, which need the paid features", null, { v: 1.36, p90: 2.43, p99: 9.4 }, null] },
            { label: "vector", cells: [{ v: 0.58, p90: 0.63, p99: 0.9 }, { v: 2.12, p90: 3.62, p99: 9.4 }, "skip:only the hybrid kinds, which need the paid features", null, { v: 2.54, p90: 3.41, p99: 7.5 }, null] },
            { label: "vector + filter", cells: [{ v: 0.75, p90: 0.82, p99: 1.2 }, { v: 1.86, p90: 2.39, p99: 4.2 }, "skip:only the hybrid kinds, which need the paid features", null, { v: 4.92, p90: 7.01, p99: 12.8 }, null] },
            { label: "hybrid, RRF", cells: [{ v: 1.82, p90: 2.09, p99: 2.6 }, "na:not on the free licence", null, null, { v: 3.32, p90: 5.23, p99: 11.3 }, null] },
            { label: "hybrid, weighted", cells: [{ v: 1.83, p90: 2.18, p99: 2.9 }, { v: 2.42, p90: 3.77, p99: 8.1, note: "a raw-score sum: no normalised fusion on the free licence" }, null, null, { v: 3.47, p90: 4.84, p99: 10.0, note: "a raw-score sum" }, null] },
            { label: "boosted: BM25 × date decay", cells: [{ v: 1.19, p90: 1.47, p99: 2.0 }, { v: 1.59, p90: 3.06, p99: 6.1 }, "skip:only the hybrid kinds, which need the paid features", null, { v: 1.55, p90: 3.27, p99: 6.4 }, null] },
            { label: "facet", cells: [{ v: 0.41, p90: 0.46, p99: 0.7 }, { v: 0.82, p90: 1.44, p99: 3.7 }, "skip:only the hybrid kinds, which need the paid features", null, { v: 0.78, p90: 1.51, p99: 5.0 }, null] },
            { label: "key lookup", cells: [{ v: 0.32, p90: 0.41, p99: 1.2 }, { v: 0.65, p90: 0.79, p99: 1.2 }, "skip:only the hybrid kinds, which need the paid features", null, { v: 0.81, p90: 0.97, p99: 2.0 }, null] },
            { label: "phrase", s: "placeholder", cells: [null, null, "skip:only the hybrid kinds, which need the paid features", null, null, null] },
            { label: "typo-tolerant", s: "placeholder", cells: [null, null, "skip:only the hybrid kinds, which need the paid features", null, null, null] },
            { label: "prefix autocomplete", s: "placeholder", cells: [null, null, "skip:only the hybrid kinds, which need the paid features", null, null, null] },
            { label: "similar items with exclusions", s: "placeholder", cells: [null, null, "skip:only the hybrid kinds, which need the paid features", null, null, null] },
            { label: "SQL aggregate: GROUP BY two fields, HAVING", s: "placeholder", cells: [null, null, "skip:only the hybrid kinds, which need the paid features", null, null, null] }
          ]
        },
        {
          id: "b-qps", title: "Sixteen clients", sub: "Queries a second, higher is better", unit: "q/s", digits: 0, better: "higher", scale: "row", status: "earlier", run: "m2-kind-2026-10-28",
          columns: [{ name: "Strata", ver: "2 shards + coordinator", self: true }, { name: "Elasticsearch", ver: "9.5.5" }, { name: "Elasticsearch", ver: "trial licence", status: "placeholder" }, { name: "OpenSearch", ver: "3", status: "placeholder" }, { name: "Solr", ver: "10.1" }, { name: "Vespa", ver: "8", status: "placeholder" }],
          rows: [
            { label: "BM25", cells: [3058, 1162, "skip:only the hybrid kinds, which need the paid features", null, 1114, null] },
            { label: "BM25 + filter", cells: [2003, 1440, "skip:only the hybrid kinds, which need the paid features", null, 1886, null] },
            { label: "vector", cells: [3366, 1256, "skip:only the hybrid kinds, which need the paid features", null, 584, null] },
            { label: "vector + filter", cells: [2087, 1148, "skip:only the hybrid kinds, which need the paid features", null, 281, null] },
            { label: "hybrid, RRF", cells: [849, "na:not on the free licence", null, null, 469, null] },
            { label: "hybrid, weighted", cells: [759, 707, null, null, 419, null] },
            { label: "boosted: BM25 × date decay", cells: [1492, 1087, "skip:only the hybrid kinds, which need the paid features", null, 1030, null] },
            { label: "facet", cells: [6158, 6272, "skip:only the hybrid kinds, which need the paid features", null, 3180, null] },
            { label: "key lookup", cells: [11158, 8137, "skip:only the hybrid kinds, which need the paid features", null, 3259, null] }
          ],
          note: "At 16 clients every engine is throttled by its CPU quota, and the client shares the same 4 CPUs. Elasticsearch ties Strata on facets."
        },
        {
          id: "b-quality", title: "The same answers", sub: "nDCG@10 against Quora's judgments, or recall@10 against an exact search; higher is better", unit: "", digits: 3, better: "higher", scale: "unit", status: "earlier", run: "m2-kind-2026-10-28",
          columns: [{ name: "Strata", self: true }, { name: "Elasticsearch", ver: "9.5.5" }, { name: "Elasticsearch", ver: "trial licence", status: "placeholder" }, { name: "OpenSearch", ver: "3", status: "placeholder" }, { name: "Solr", ver: "10.1" }, { name: "Vespa", ver: "8", status: "placeholder" }],
          rows: [
            { label: "BM25, nDCG@10", cells: [0.790, 0.788, "skip:only the hybrid kinds, which need the paid features", null, 0.793, null] },
            { label: "vector, recall@10", cells: [0.986, 0.984, "skip:only the hybrid kinds, which need the paid features", null, 0.993, null] },
            { label: "vector + filter, recall@10", cells: [0.991, 0.934, "skip:only the hybrid kinds, which need the paid features", null, 0.998, null] },
            { label: "hybrid RRF, nDCG@10", cells: [0.863, "na:not on the free licence", null, null, 0.871, null] },
            { label: "hybrid weighted, nDCG@10", cells: [0.905, { v: 0.827, note: "a raw-score sum" }, null, null, { v: 0.823, note: "a raw-score sum" }, null] }
          ],
          note: "Strata's weighted hybrid normalises each score before summing (minmax); the others can only add raw scores."
        },
        {
          id: "b-load", title: "Load and size", sub: "Lower is better", unit: "", better: "lower", scale: "row", status: "earlier", run: "m2-kind-2026-10-28",
          columns: [{ name: "Strata", self: true }, { name: "Elasticsearch", ver: "9.5.5" }, { name: "OpenSearch", ver: "3", status: "placeholder" }, { name: "Solr", ver: "10.1" }, { name: "Vespa", ver: "8", status: "placeholder" }],
          rows: [
            { label: "load to one searchable segment", unit: "s", digits: 0, cells: [225, 549, null, 1087, null] },
            { label: "index size", unit: "GB", digits: 2, cells: [2.82, 2.78, null, 2.24, null] }
          ],
          note: "Strata and Elasticsearch keep full vectors and 8-bit codes; Solr keeps full vectors only."
        },
        {
          id: "b-ops", title: "Operations", sub: "Measured while 4 clients query", unit: "", better: "lower", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", self: true }, { name: "Elasticsearch", ver: "9" }, { name: "OpenSearch", ver: "3" }, { name: "Solr", ver: "10" }, { name: "Vespa", ver: "8" }],
          rows: [
            { label: "scale out 2 → 3 shards: time until balanced", unit: "s", cells: [null, null, null, null, null] },
            { label: "query p99 during the move", unit: "ms", cells: [null, null, null, null, null] },
            { label: "failover: failed queries", unit: "", cells: [null, null, null, null, null] },
            { label: "failover: time until healthy", unit: "s", cells: [null, null, null, null, null] },
            { label: "memory", unit: "GiB", cells: [null, null, null, null, null] },
            { label: "pods", unit: "", cells: [null, null, null, null, null] }
          ]
        },
        {
          id: "b-writes", title: "Writes on the cluster", sub: "Single-document writes after the bulk load; documents a second, higher is better", unit: "docs/s", better: "higher", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", ver: "/v1/write", self: true }, { name: "Elasticsearch", ver: "9, index API" }, { name: "OpenSearch", ver: "3, index API" }, { name: "Solr", ver: "10, update handler" }, { name: "Vespa", ver: "8, /document/v1" }],
          rows: [
            { label: "1 client", cells: [null, null, null, null, null] },
            { label: "4 clients", cells: [null, null, null, null, null] },
            { label: "16 clients", cells: [null, null, null, null, null] },
            { label: "p50 per write", unit: "ms", better: "lower", cells: [null, null, null, null, null] },
            { label: "p99 per write", unit: "ms", better: "lower", cells: [null, null, null, null, null] },
            { label: "time until a write is visible", unit: "ms", better: "lower", cells: [null, null, null, null, null] },
            { label: "read p99 at 500 writes a second, 4 clients querying", unit: "ms", better: "lower", cells: [null, null, null, null, null] }
          ],
          note: "Elasticsearch and OpenSearch at their 1 s refresh, Solr at a 1 s soft commit, Vespa and Strata as committed. Every write is read back."
        },
        {
          id: "b-updates", title: "Updates on the cluster", sub: "Documents a second, higher is better; latency rows lower is better", unit: "docs/s", better: "higher", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", ver: "UPDATE", self: true }, { name: "Elasticsearch", ver: "9, _update" }, { name: "OpenSearch", ver: "3, _update" }, { name: "Solr", ver: "10, in-place" }, { name: "Vespa", ver: "8, partial update" }],
          rows: [
            { label: "a rating by id", cells: [null, null, null, null, null] },
            { label: "a body changed (re-indexed)", cells: [null, null, null, null, null] },
            { label: "p99 per update", unit: "ms", better: "lower", cells: [null, null, null, null, null] },
            { label: "time until an update is visible", unit: "ms", better: "lower", cells: [null, null, null, null, null] }
          ]
        },
        {
          id: "b-stream", title: "A stream into the cluster", sub: "A Redpanda topic of 4 partitions, while 4 clients query", unit: "ms", better: "lower", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", ver: "its own consumer", self: true }, { name: "Elasticsearch", ver: "9, a consumer + bulk" }, { name: "OpenSearch", ver: "3, a consumer + bulk" }, { name: "Solr", ver: "10, a consumer + update" }, { name: "Vespa", ver: "8, a consumer + feed" }],
          rows: [
            { label: "events a second, flat out", unit: "ev/s", better: "higher", cells: [null, null, null, null, null] },
            { label: "time until visible at 5K events a second", cells: [null, null, null, null, null] },
            { label: "read p99 at 5K events a second", cells: [null, null, null, null, null] },
            { label: "read p99 at 20K events a second", cells: [null, null, null, null, null] },
            { label: "events lost after a consumer restart", unit: "", cells: [null, null, null, null, null] },
            { label: "events repeated after a consumer restart", unit: "", cells: [null, null, null, null, null] }
          ],
          note: "Strata keeps the topic's offsets in the same transaction as the rows, so a restart should lose and repeat nothing; the others' at-least-once delivery is counted as it comes out."
        },
        {
          id: "b-operators", type: "ops", title: "Operator by operator, on the cluster", sub: "What each engine can run, and its p50 in ms at one client where it can", unit: "ms", status: "placeholder",
          columns: [{ name: "Strata", self: true }, { name: "Elasticsearch", ver: "9" }, { name: "OpenSearch", ver: "3" }, { name: "Solr", ver: "10" }, { name: "Vespa", ver: "8" }],
          rows: [
            { group: "Words" },
            { label: "AND, OR, NOT, nested", cells: ["y", "y", "y", "y", "y"] },
            { label: "word weights", cells: ["y", "y", "y", "y", "y"] },
            { label: "at least n of the words", cells: ["y", "y", "y", "y", "n:weakAnd only approximates it"] },
            { label: "at least n of several conditions", cells: ["y", "y", "y", "y", "n"] },
            { label: "phrase", cells: ["y", "y", "y", "y", "y"] },
            { label: "words near each other", cells: ["n:not yet", "y", "y", "y", "y"] },
            { label: "prefix of a word in text", cells: ["y", "y", "y", "y", "n:on attributes only"] },
            { label: "typos in text", cells: ["y", "y", "y", "y:fuzzy words are not stemmed", "n:on attributes only"] },
            { label: "autocomplete with typos", cells: ["y", "y:completion suggester", "y", "y:suggester", "c"] },
            { label: "query-time synonyms", cells: ["y", "y", "y", "y", "y"] },
            { label: "highlighted snippets", cells: ["n:not yet", "y", "y", "y", "y"] },
            { group: "Filters and ranking" },
            { label: "range, IN, NOT IN, IS NULL", cells: ["y", "y", "y", "y", "p:no NULL"] },
            { label: "JSON paths of keys no schema names", data: "shop", cells: ["y", "p:flattened, keyword only", "p:flat_object", "p:dynamic fields", "p:map attributes"] },
            { label: "geo distance", cells: ["n:not yet", "y", "y", "y", "y"] },
            { label: "hybrid RRF", cells: ["y", "$:RRF retriever; run on a trial licence", "y", "y", "y"] },
            { label: "normalised weighted fusion", cells: ["y", "$:linear retriever with min-max; run on a trial licence", "y", "n", "y:min-max"] },
            { label: "boosts by a column or a decay", cells: ["y", "y", "y", "y", "y"] },
            { label: "sparse vectors (SPLADE)", data: "shop", cells: ["y", "y", "y", "n", "y"] },
            { label: "several query vectors (MaxSim)", data: "shop", cells: ["y", "c:licence", "c", "n", "y"] },
            { label: "at most k rows per value", cells: ["y", "y:collapse", "y:collapse", "y:collapse", "y"] },
            { group: "SQL and analytics" },
            { label: "GROUP BY two fields with HAVING", cells: ["y", "y", "y", "y", "p:no HAVING"] },
            { label: "exact COUNT(DISTINCT) over shards", cells: ["y", "p:approximate", "p:approximate", "p:approximate", "c"] },
            { label: "percentiles", cells: ["n:not yet", "y", "y", "y", "c"] },
            { label: "joins of two tables", data: "shop", cells: ["y", "c:ES|QL lookup join", "p:SQL plugin", "p:filter only", "p:parent fields"] },
            { label: "subqueries, correlated subqueries", data: "shop", cells: ["y", "n", "n", "n", "n"] },
            { label: "graph walks over an edge table", data: "shop", cells: ["y", "n", "n", "p:streaming expressions", "n"] },
            { label: "window functions, UNION", cells: ["n:until DataFusion's next step", "n", "n", "c", "n"] }
          ],
          note: "Marks come from each engine's documentation today; the runs confirm or correct each one. Rows marked shop need fields Quora lacks and run on the shop data, loaded into every engine. Strata's own gaps stay in the table."
        },
        {
          id: "b2-marco", title: "Larger than memory: MS MARCO, 2M passages", sub: "One client, median latency in ms, lower is better", unit: "ms", better: "lower", scale: "row", status: "placeholder",
          columns: [{ name: "Strata", self: true }, { name: "Elasticsearch", ver: "9" }, { name: "OpenSearch", ver: "3" }, { name: "Solr", ver: "10" }, { name: "Vespa", ver: "8" }],
          rows: [
            { label: "BM25", cells: [null, null, null, null, null] },
            { label: "vector", cells: [null, null, null, null, null] },
            { label: "vector + filter", cells: [null, null, null, null, null] },
            { label: "hybrid, weighted", cells: [null, null, null, null, null] }
          ]
        }
      ]
    }
  ],

  /* ------------------------------------------------------------- C */
  stages: {
    run: null,
    status: "placeholder",
    note: "The same shop file and the same 12 app queries at each stage; every answer is checked against stage 1, row by row.",
    metrics: ["setup time", "p50 per query", "answers identical to stage 1", "memory", "disk"],
    rows: [
      { stage: "In the app", detail: "Python opens shop.strata in-process", values: [null, null, null, null, null] },
      { stage: "One server", detail: "the same file under strata-server, over HTTP", values: [null, null, null, null, null] },
      { stage: "A cluster on kind", detail: "the same tables on 2 shards; the operator grows it to 3 while the queries run", values: [null, null, null, null, null] },
      { stage: "The developer loop", detail: "git clone to a running 3-shard cluster on the laptop", values: [null, null, null, null, null] }
    ]
  }
};
