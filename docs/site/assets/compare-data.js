/* The capability comparison on compare.html.
 *
 * A cell is a code, optionally "?" (not yet checked against that project's
 * current documentation), optionally ":" and a note shown on hover:
 *   y  yes, built in         p  partial, by extension, or by hand
 *   n  no                    $  only in a paid tier
 *   t  text (the licence row)
 * Systems missing from a row read as "n".
 *
 * DRAFT, 1 November 2026: every "?" cell, and every claim about another
 * system, is to be checked against its current documentation before this
 * page is published.
 */
window.STRATA_COMPARE = {
  systems: [
    { id: "strata", name: "Strata", sub: "embedded or server", group: "self" },
    { id: "sqlite", name: "SQLite", sub: "FTS5, sqlite-vec", group: "embedded" },
    { id: "postgres", name: "PostgreSQL", sub: "pgvector", group: "embedded" },
    { id: "lancedb", name: "LanceDB", sub: "embedded", group: "embedded" },
    { id: "elastic", name: "Elasticsearch", sub: "9, Basic licence", group: "search" },
    { id: "opensearch", name: "OpenSearch", sub: "3", group: "search" },
    { id: "solr", name: "Solr", sub: "10", group: "search" },
    { id: "vespa", name: "Vespa", sub: "8", group: "search" },
    { id: "qdrant", name: "Qdrant", sub: "", group: "vector" },
    { id: "weaviate", name: "Weaviate", sub: "", group: "vector" },
    { id: "milvus", name: "Milvus", sub: "", group: "vector" }
  ],
  groups: [
    { id: "all", label: "All" },
    { id: "search", label: "Search engines" },
    { id: "vector", label: "Vector databases" },
    { id: "embedded", label: "Embedded and relational" }
  ],
  sections: [
    {
      title: "Where it runs",
      rows: [
        ["Runs inside your program, as a library", {
          strata: "y:A Rust crate, or Python in-process like sqlite3", sqlite: "y", postgres: "n", lancedb: "y",
          elastic: "n", opensearch: "n", solr: "n", vespa: "n",
          qdrant: "p?:Local mode of the Python client, meant for tests", weaviate: "p?:Embedded Weaviate, experimental", milvus: "p:Milvus Lite, a separate build with the same API" }],
        ["The whole database in one file", {
          strata: "y:One file and its write-ahead log, or a directory of segments", sqlite: "y", postgres: "n", lancedb: "n:A directory of Lance files",
          milvus: "p?:Milvus Lite keeps one local file" }],
        ["A network server (HTTP or gRPC)", {
          strata: "y:HTTP JSON and gRPC, API keys with scopes, TLS", sqlite: "n", postgres: "y:Its own wire protocol", lancedb: "p?:LanceDB Cloud and Enterprise",
          elastic: "y", opensearch: "y", solr: "y", vespa: "y", qdrant: "y", weaviate: "y", milvus: "y" }],
        ["Shards and replicas", {
          strata: "y:A coordinator over shards; slots move while it serves; a primary ships writes to replicas", postgres: "p:Streaming replicas; sharding through Citus", lancedb: "p?:LanceDB Enterprise",
          elastic: "y", opensearch: "y", solr: "y:SolrCloud", vespa: "y", qdrant: "y", weaviate: "y", milvus: "y" }],
        ["Automatic failover", {
          strata: "y:A shard's copies elect a new primary; writes acknowledged by all, a majority or the primary", postgres: "p:Through Patroni or an operator",
          elastic: "y", opensearch: "y", solr: "y:Through ZooKeeper", vespa: "y", qdrant: "y:Raft", weaviate: "y?", milvus: "y" }],
        ["A Kubernetes operator", {
          strata: "y:StrataCluster: scale shards and copies, roll out settings with gates, roll back", postgres: "y:CloudNativePG and others",
          elastic: "y:ECK", opensearch: "y:OpenSearch Kubernetes Operator", solr: "y:Solr Operator", vespa: "p?:Helm charts and sample manifests; Vespa Cloud is managed",
          qdrant: "p?:A Helm chart; an operator in the commercial offering", weaviate: "p:A Helm chart", milvus: "y:Milvus Operator" }],
        ["The same API from a laptop file to a cluster", {
          strata: "y:The same SQL, schema and file format at every size", sqlite: "n:One machine", postgres: "p:The same SQL; no embedded mode", lancedb: "p?:The open-source library, then LanceDB Cloud",
          qdrant: "p:Local mode, then the server, with one client", weaviate: "p?", milvus: "p:Milvus Lite, then Milvus" }]
      ]
    },
    {
      title: "Text search",
      rows: [
        ["BM25 with boolean queries and phrases", {
          strata: "y:MATCH with AND, OR, NOT, phrases; MaxScore and Block-Max WAND", sqlite: "y:FTS5", postgres: "p:ts_rank is not BM25; BM25 through extensions such as ParadeDB", lancedb: "y?",
          elastic: "y", opensearch: "y", solr: "y", vespa: "y", qdrant: "p?:Full-text filters; BM25 as sparse vectors with IDF", weaviate: "y", milvus: "y?:A BM25 function, from 2.5" }],
        ["Typos and synonyms", {
          strata: "y:word~ for edits, synonym sets", sqlite: "p:The spellfix1 extension, by hand", postgres: "p:pg_trgm and thesaurus dictionaries", lancedb: "p?",
          elastic: "y", opensearch: "y", solr: "y", vespa: "y", weaviate: "p?", milvus: "p?" }],
        ["Autocomplete that tolerates typos", {
          strata: "y:PREFIX(..., fuzzy => 'auto'), a trie walk of the dictionary", sqlite: "p:Prefix queries in FTS5, no typos", postgres: "p:pg_trgm",
          elastic: "y:Completion suggester", opensearch: "y", solr: "y:Suggester", vespa: "p?" }],
        ["Facets and aggregations over a search", {
          strata: "y:GROUP BY over any result", sqlite: "y:GROUP BY", postgres: "y", lancedb: "p?",
          elastic: "y", opensearch: "y", solr: "y", vespa: "y:Grouping", qdrant: "p?:Facet counts", weaviate: "p:Aggregate", milvus: "p?" }]
      ]
    },
    {
      title: "Vectors",
      rows: [
        ["A vector index (HNSW or IVF)", {
          strata: "y:HNSW, IVF or exact; float32, 8-bit or binary codes", sqlite: "p?:sqlite-vec scans; no ANN index", postgres: "y:HNSW and IVFFlat", lancedb: "y",
          elastic: "y", opensearch: "y", solr: "y", vespa: "y", qdrant: "y", weaviate: "y", milvus: "y" }],
        ["Filtered vector search", {
          strata: "y:Planned per segment from exact counts: an exact scan, the filter inside HNSW, or a post-filter", sqlite: "p", postgres: "p?:Iterative index scans", lancedb: "p:Pre- or post-filter, chosen by the caller",
          elastic: "y", opensearch: "y", solr: "y", vespa: "y", qdrant: "y", weaviate: "y", milvus: "y" }],
        ["Hybrid fusion: RRF and normalised weighted sums", {
          strata: "y:rrf(), and minmax(), tmm() or zscore() in any weighted sum", sqlite: "p:Written by hand in SQL", postgres: "p:Written by hand in SQL", lancedb: "y",
          elastic: "$:RRF and linear retrievers need a paid licence; the free Basic licence refused RRF on 28 October",
          opensearch: "y:Normalisation and RRF processors in search pipelines", solr: "p?:RRF in Solr 10; no normalised sum", vespa: "y:Any ranking expression",
          qdrant: "y:RRF and DBSF", weaviate: "y:Ranked and relative-score fusion", milvus: "y:RRF and weighted rankers" }],
        ["Multi-vector late interaction (ColBERT, ColPali)", {
          strata: "y:maxsim() over VECTOR(d)[] fields, with token pooling", postgres: "p?:Through extensions such as VectorChord", lancedb: "y?",
          elastic: "p?:The rank_vectors field", opensearch: "n?", vespa: "y", qdrant: "y", weaviate: "y?", milvus: "p?" }],
        ["Learned sparse vectors (SPLADE, ELSER)", {
          strata: "y:SPARSE fields", postgres: "y:pgvector's sparsevec", lancedb: "n?",
          elastic: "y", opensearch: "y", solr: "n?", vespa: "y", qdrant: "y", weaviate: "n?", milvus: "y" }],
        ["Embeddings computed on write", {
          strata: "y:Local text, image and sound models, or OpenAI, Cohere, Voyage, Gemini, Mistral, Jina, Ollama; missing vectors backfilled", postgres: "p?:Extensions such as pgai", lancedb: "y:Embedding functions",
          elastic: "y:semantic_text and the inference API", opensearch: "y:Ingest pipelines with ML Commons", solr: "p?", vespa: "y:Embedders",
          qdrant: "p?:Client-side FastEmbed, or cloud inference", weaviate: "y:Vectorizer modules", milvus: "y?:Embedding functions" }]
      ]
    },
    {
      title: "Ranking",
      rows: [
        ["Ranking expressions over scores and columns", {
          strata: "y:Any arithmetic of scores and columns, decay(), rank-safe boosts, rerank phases", sqlite: "p:ORDER BY expressions over bm25()", postgres: "p", lancedb: "p?",
          elastic: "y:function_score, script_score", opensearch: "y", solr: "y:Function queries", vespa: "y", qdrant: "p?:Formula queries", weaviate: "n?", milvus: "p?" }],
        ["Gradient-boosted ranking models (LightGBM, XGBoost)", {
          strata: "y:gbdt() in a rerank phase, bit-exact with LightGBM and XGBoost",
          elastic: "$?:Learning to rank", opensearch: "y:The LTR plugin", solr: "y:The LTR module", vespa: "y" }],
        ["Cross-encoder reranking inside the query", {
          strata: "y:ONNX Runtime (int8) or candle locally, or Cohere, Voyage, Jina, TEI", lancedb: "y:Rerankers",
          elastic: "$?:text_similarity_reranker with the inference API", opensearch: "y", solr: "p?", vespa: "y", weaviate: "y:Reranker modules", milvus: "y?" }],
        ["Rank log, interleaving and LLM relevance judgments", {
          strata: "y:Logged features for training, team-draft interleaving, UMBRELA judgments by Claude",
          opensearch: "p?:Search Relevance Workbench" }]
      ]
    },
    {
      title: "Recommendations",
      rows: [
        ["Sequence models in the query (the next item from a history)", {
          strata: "y:encode() runs a SASRec-style model trained with strata.train", vespa: "p?:ONNX models in ranking" }],
        ["Items like a user's recent history", {
          strata: "y:maxsim() over the history's vectors, or ppr() over co-occurrence edges", postgres: "p:Hand-written SQL", lancedb: "p",
          elastic: "p", opensearch: "p", vespa: "y", qdrant: "y:The recommend API", weaviate: "p?", milvus: "p?" }],
        ["Semantic IDs", {
          strata: "y:Residual-quantised IDs per row, and the next codes of a prefix" }],
        ["Counted next items, decayed in place", {
          strata: "y:strata.next_items over an n-gram table; UPDATE ... SET n = n * 0.9 rewrites no row" }]
      ]
    },
    {
      title: "SQL and data",
      rows: [
        ["SQL with joins, subqueries and WITH", {
          strata: "y:With DataFusion as the engine of compound plans", sqlite: "y", postgres: "y", lancedb: "p?:SQL through DataFusion or DuckDB",
          elastic: "p:ES|QL and SQL, with lookup joins", opensearch: "p:The SQL and PPL plugin", solr: "p:Parallel SQL", vespa: "p:YQL, without joins",
          weaviate: "n:GraphQL" }],
        ["ACID transactions with snapshot isolation", {
          strata: "y:Shadow-paged commits; readers never wait for the writer", sqlite: "y", postgres: "y", lancedb: "p?:Versioned commits per table",
          elastic: "n:Near real time, versions per document" }],
        ["Graph traversal and personalized PageRank", {
          strata: "y:REACHABLE, PATH and ppr() over edge fields or edge tables, seeded by any query", sqlite: "p:Recursive CTEs", postgres: "p:Recursive CTEs, or Apache AGE",
          solr: "p:The graph query parser and streaming expressions" }],
        ["Schemaless JSON attributes, indexed", {
          strata: "y:JSON fields, filtered and grouped by path, listed by json_paths()", sqlite: "p:JSON functions and indexes on expressions", postgres: "y:jsonb with GIN", lancedb: "p?",
          elastic: "y:Dynamic mapping", opensearch: "y", solr: "y:Dynamic fields", vespa: "n?", qdrant: "y:Payload", weaviate: "p:Auto-schema", milvus: "y:JSON and dynamic fields" }],
        ["Change columns while serving", {
          strata: "y:ALTER TABLE ADD, DROP and RENAME COLUMN, across a cluster", sqlite: "y", postgres: "y", lancedb: "y?",
          elastic: "p:Add fields; other changes reindex", opensearch: "p", solr: "p", vespa: "p:Some changes need a reindex", qdrant: "y:The payload has no schema", weaviate: "p?", milvus: "p?" }],
        ["Import from and export to SQLite", {
          strata: "y:strata-server import and export, or one call from Python", sqlite: "y", postgres: "p:pgloader" }],
        ["Images and sound by reference, embedded on write", {
          strata: "y:MEDIA fields; SigLIP and CLAP models; captions made into text", lancedb: "y:Multimodal tables",
          elastic: "p?", opensearch: "p?:A multimodal ingest processor", vespa: "p?:Embedders over images", qdrant: "p?", weaviate: "y:multi2vec modules", milvus: "p?" }]
      ]
    },
    {
      title: "Operations",
      rows: [
        ["A console with query profiles and flame graphs", {
          strata: "y:The command center, served by the server itself", postgres: "p:EXPLAIN ANALYZE; consoles are separate",
          elastic: "y:Kibana, a separate service; the profile API", opensearch: "y:OpenSearch Dashboards, a separate service", solr: "y:The admin UI",
          vespa: "p?:Tracing; no bundled console", qdrant: "y?:A web UI", milvus: "p:Attu, a separate service" }],
        ["Plans a cloud cluster with its cost", {
          strata: "y:The Cloud view plans EKS or DigitalOcean Kubernetes from live prices and quotas" }],
        ["Licence", {
          strata: "t:Apache-2.0", sqlite: "t:Public domain", postgres: "t:PostgreSQL", lancedb: "t:Apache-2.0",
          elastic: "t:AGPLv3, SSPL or ELv2", opensearch: "t:Apache-2.0", solr: "t:Apache-2.0", vespa: "t:Apache-2.0",
          qdrant: "t:Apache-2.0", weaviate: "t:BSD-3-Clause", milvus: "t:Apache-2.0" }]
      ]
    }
  ]
};
