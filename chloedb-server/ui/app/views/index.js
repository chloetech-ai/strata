// The views, and the navigation: Build (data in, search, saved searches),
// Watch (is it healthy), Run the cluster (its machines and settings), the
// apps, then Labs (tools for search engineers), collapsed and pinnable.
//
// A view module exports { id, title, group, icon, render(el, route), onParams?,
// badge?() -> { n, tone } | null, keywords?, full? }. NAV fixes the order, the
// names the navigation shows and the shortcuts; a view a module does not
// provide (yet) is left out of the navigation.
//
// CCW-10: a view's code loads when it is first opened (or hovered in the
// navigation), not with the shell. VIEWS holds what the shell needs before
// that: each view's id, title, icon, how its page is laid out (full, range,
// cluster, parent) and the words the palette finds it by, copied from the
// module's own export (tests/ui_numbers.mjs checks they agree), and load(),
// which imports it.

export const VIEWS = [
  { id: "home", title: "Home", icon: "overview", group: "Operate", keywords: "overview home fleet health status incident banner problems slo where capacity coordinators affected dashboard morning check", load: () => import("./overview.js") },
  { id: "timeline", title: "Timeline", icon: "timeline", group: "Operate", keywords: "timeline events history what changed overnight failover primary copy down rollout audit changes notes incident postmortem", load: () => import("./timeline.js") },
  { id: "alerts", title: "Alerts", icon: "alerts", group: "Operate", keywords: "alerts rules firing incidents acknowledge silence runbook owner routes webhook notifications backtest", load: () => import("./alerts.js") },
  { id: "oncall", title: "On-call agent", icon: "agent", group: "Operate", keywords: "on-call oncall agent claude ai llm investigate incident alert firing why slow root cause diagnose triage proposals approve reject contain cancel silence runbook change request changes inbox dry run plan stale", load: () => import("./oncall.js") },
  { id: "cluster", title: "Cluster", icon: "layers", group: "Operate", range: false, keywords: "cluster canvas fleet topology control center shards copies replicas nodes zones health cpu disk p99 version lag what if fail draft plan apply add remove promote primary drain move slots rebalance capacity autoscale changes events kubernetes operator", load: () => import("./cluster.js") },
  { id: "logs", title: "Logs", icon: "logs", group: "Operate", keywords: "log lines errors warnings request id level debug search patterns live tail server copy shard route status", load: () => import("./logs.js") },
  { id: "explorer", title: "Log explorer", icon: "search", group: "Operate", cluster: "brief", keywords: "log explorer logs syslog otlp opentelemetry collector search grep tail live histogram patterns surrounding lines context host interface ip error last 15 minutes incident oncall", load: () => import("./explorer.js") },
  { id: "queries", title: "Queries", icon: "queries", group: "Operate", keywords: "live running cancel recent latest slow statements history trace waterfall contain limit refuse throttle key client pg_stat_statements", load: () => import("./queries.js") },
  { id: "statement", title: "Statement", icon: "queries", group: "Operate", parent: "queries", nav: false, hidden: true, keywords: "statement fingerprint shape", load: () => import("./statement.js") },
  { id: "performance", title: "Performance", icon: "perf", group: "Operate", keywords: "latency throughput heatmap findings advisor tuning cpu memory disk routes kinds percentiles slow copies fleet failures", load: () => import("./performance.js") },
  { id: "profile", title: "Profiler", icon: "flame", group: "Diagnose", keywords: "profile profiler diagnostics node explain analyze flame chart flame graph cpu sampling pprof perf folded speedscope hot functions memory heap allocations allocator leak rss threads context switches segments operators steps debug latency settings slow threshold", load: () => import("./profile.js") },
  { id: "querypath", title: "Query path", icon: "route", group: "Diagnose", range: false, cluster: "full", keywords: "query path fan out fan-out scatter gather tail latency straggler hedge hedging shards radar slowest shard p99 params", load: () => import("./querypath.js") },
  { id: "tables", title: "Tables and storage", icon: "layers", group: "Diagnose", range: false, keywords: "tables storage size bytes rows growth growing skew skewed shard hot reads writes stale lag copies replicas disk inspector index", load: () => import("./tables.js") },
  { id: "inspector", title: "Table details", icon: "layers", group: "Diagnose", parent: "tables", nav: false, range: false, keywords: "index inspector table details statistics fields documents size bytes distribution histogram freshness last write segments retrieval reads keys coverage hot shards replicas skew", load: () => import("./inspector.js") },
  { id: "rollout", title: "Rollouts", icon: "rollout", group: "Change", keywords: "rollout roll out upgrade upgrader new version versions picker image tag commit digest registry ghcr automatic history rollback roll back undo pause resume hold held gate gates health soak verify preflight progress eta pace restart operator kubernetes", load: () => import("./rollout.js") },
  { id: "autoscale", title: "Autoscaling", icon: "autoscale", group: "Change", keywords: "autoscale autoscaling autoscaler scale up down out in grow shrink copies replicas busy cpu cores volumes disk full freeze cooldown rules replay week suggest approve kubernetes operator", load: () => import("./autoscale.js") },
  { id: "simulator", title: "Simulator", icon: "gauge", group: "Change", keywords: "simulator simulate test run load test benchmark traffic mix qps capacity throughput latency p99 slo replicas copies shards machines cost estimate what if predict model grade advice", load: () => import("./simulator.js") },
  { id: "access", title: "Access and audit", icon: "access", group: "Change", keywords: "keys api key token scopes audit log changes who changed refused refusals security users permissions rotate revoke owner", load: () => import("./access.js") },
  { id: "system", title: "Settings", icon: "system", group: "Change", keywords: "settings flags spec statefulset write acks failover hedge timeout slow limits resources memory cores volume storage token coordinators upkeep verify compare replicas checkpoint backup vacuum check snapshot lease reset statistics process limits segments merges config metrics prometheus disk", load: () => import("./system.js") },
  { id: "cloud", title: "Cloud setup", icon: "cloud", group: "Change", keywords: "cloud aws amazon eks digitalocean doks droplet kubernetes k8s credentials access key token region instance instances machine machines size sizes ec2 graviton price prices cost spot quota limit cloudformation template deploy", load: () => import("./cloud.js") },
  { id: "connections", title: "Connections", icon: "key", group: "Change", keywords: "connections hub clusters multi cluster book keys keychain remote cloud switch open another cluster certificate pin tls fingerprint", load: () => import("./connections.js") },
  { id: "clusters", title: "All clusters", icon: "cluster", group: "Change", keywords: "all clusters fleet many clusters hub overview health drift compare config settings synonyms alert rules tables schema apply sync revoke device", load: () => import("./clusters.js") },
  { id: "follow", title: "Follow", icon: "share", group: "Change", keywords: "follow follower source replicate cluster copy feed lag behind switch over switchover failover promote hold writes pause resume stop disaster recovery standby", load: () => import("./follow.js") },
  { id: "sql", title: "SQL workbench", icon: "sql", group: "Data", full: true, range: false, keywords: "query editor sql run explain plan results chart history saved snippets parameters", load: () => import("./sql.js") },
  { id: "schema", title: "Schema", icon: "schema", group: "Data", full: true, range: false, keywords: "tables fields diagram er entity relationship references edges embeddings create table designer ddl drop", load: () => import("./schema.js") },
  { id: "models", title: "Models", icon: "cpu", group: "Data", range: false, keywords: "models model embedding embeddings embedder vector vectors field technique static model2vec potion int8 quantize quantized bert transformer colbert late interaction multi-vector hosted openai hnsw ivf flat sq8 binary rabitq codes backfill change switch compare fetch", load: () => import("./models.js") },
  { id: "import", title: "Import and export", icon: "import", group: "Data", range: false, keywords: "import upload load csv tsv json jsonl ndjson sqlite parquet arrow spark file export download stream kafka sources offsets", load: () => import("./import.js") },
  { id: "slices", title: "Slices", icon: "layers", group: "Data", range: false, keywords: "slices slice device devices phone offline on-device edge tiles tile map geo near nearby location vector vectors cluster clusters taste similar recommendations manifest download cloud fallback proof proved fresh stale rebuild", load: () => import("./slices.js") },
  { id: "builder", title: "Query builder", icon: "builder", group: "Labs", full: true, range: false, keywords: "visual query builder drag drop filter facet boost hybrid fusion rrf group no code", load: () => import("./builder.js") },
  { id: "relevance", title: "Relevance lab", icon: "relevance", group: "Labs", full: true, range: false, keywords: "relevance ranking compare a/b judgments ndcg mrr precision tune weights hybrid fusion rrf bm25 vector rank profile evaluation", load: () => import("./relevance.js") },
  { id: "autopilot", title: "Relevance autopilot", icon: "relevance", group: "Labs", range: false, keywords: "relevance autopilot ranking tune proposal ndcg proof rank profile rank log judgments replay held out sign test", load: () => import("./autopilot.js") },
  { id: "recsys", title: "Recsys eval", icon: "recsys", group: "Labs", full: true, range: false, keywords: "recommender recsys evaluation rank log a/b test variants ndcg hit rate mrr ctr position bias labels events clicks purchases offline metrics training export features auc", load: () => import("./recsys.js") },
  { id: "sid", title: "Semantic IDs", icon: "sid", group: "Labs", full: true, range: false, keywords: "semantic ids codebook residual quantization rq-vae generative retrieval recommender beam search decoding prefix codes clusters sunburst", load: () => import("./sid.js") },
  { id: "autocomplete", title: "Autocomplete", icon: "autocomplete", group: "Labs", full: true, range: false, keywords: "autocomplete typeahead suggest prefix complete search as you type typos fuzzy query completion keystroke latency", load: () => import("./autocomplete.js") },
  { id: "understand", title: "Understanding", icon: "sparkle", group: "Labs", full: true, range: false, keywords: "query understanding content understanding enrich rows enrich queries features extract fields workflow pipeline models llm embed backfill classify classifier synonyms mine spell correct did you mean chunks chunking derive keyphrases language entities expand rm3 understand boost filter", load: () => import("./understand.js") },
  { id: "media", title: "Media search", icon: "media", group: "Labs", full: true, range: false, keywords: "media images photos pictures audio sound search by text image similarity siglip clap more like this embedding map visual search", load: () => import("./media.js") },
  { id: "graph", title: "Graph explorer", icon: "graph", group: "Labs", full: true, range: false, keywords: "graph network nodes edges links neighbours force layout pagerank ppr path reachable hops edge table co-occurrence similarity explore", load: () => import("./graph.js") },
  { id: "classic", title: "Query console", icon: "classic", group: "Labs", full: true, range: false, keywords: "builder graph sql editor agent ask schema import query language guide old", load: () => import("./classic.js") },
  { id: "app", title: "Apps", icon: "box", group: "Apps", range: false, keywords: "apps app plugin crawler sheets spreadsheet", load: () => import("./apps.js") },
  { id: "feedback", title: "Feedback", icon: "feedback", group: "Change", range: false, keywords: "feedback screenshot annotate annotation bug bugs report idea ideas request requests change wish triage priority prioritize backlog issue issues ux design what is wrong comment", load: () => import("./feedback.js") },
  { id: "start", title: "Start here", icon: "rocket", group: "Build", range: false, keywords: "start getting started first steps onboarding sample data tutorial quickstart sandbox connect code agent mcp", load: () => import("./start.js") },
  { id: "table", title: "Tables", icon: "table", group: "Build", range: false, keywords: "tables table home search find rows data quality filled empty used by keys storage index meaning vectors semantic", load: () => import("./table.js") },
  { id: "saved", title: "Saved searches", icon: "bookmark", group: "Build", range: false, keywords: "saved searches stored functions function endpoint api mcp tool reuse version rollback create function returns table", load: () => import("./saved.js") },
  { id: "connectors", title: "Connectors", icon: "streams", group: "Data", keywords: "connectors data sources ingest prometheus jira servicenow oxidized email mail webhook alertmanager rest api pull push cdc health", load: () => import("./connectors.js") },
];

export const GROUPS = ["Build", "Watch", "Run the cluster", "Apps", "Labs"];

/**
 * The navigation, in order: [group, [{id, title, icon, key, scope, cluster}]].
 * `key` is the letter after "g"; `scope` the key scope that changes things
 * there (the entry says so when the signed-in key has less); `cluster`: a
 * page about a cluster's shards and copies, left out of the sidebar on a
 * single server (the palette still finds it). The shell puts Build first
 * on a single server and Watch first on a coordinator.
 */
export const NAV = [
  ["Build", [
    { id: "start", title: "Start here", icon: "rocket", key: "g" },
    { id: "table", title: "Tables", icon: "table", key: "e" },
    { id: "saved", title: "Saved searches", icon: "bookmark", key: "v" },
    { id: "builder", title: "Query builder", icon: "builder", key: "b" },
    { id: "sql", title: "SQL workbench", icon: "sql", key: "w" },
    { id: "import", title: "Import and export", icon: "import", key: "i", scope: "write" },
    { id: "connectors", title: "Connectors", icon: "streams", scope: "admin" },
    { id: "models", title: "Models", icon: "cpu" },
    { id: "schema", title: "Schema", icon: "schema", key: "d" },
    { id: "slices", title: "Slices", icon: "layers" },
  ]],
  ["Watch", [
    { id: "home", title: "Home", icon: "overview", key: "h" },
    { id: "alerts", title: "Alerts", icon: "alerts", key: "a" },
    { id: "logs", title: "Logs", icon: "logs", key: "l" },
    { id: "timeline", title: "Timeline", icon: "timeline", key: "t" },
    { id: "explorer", title: "Log explorer", icon: "search", key: "m" },
    { id: "queries", title: "Queries", icon: "queries", key: "q" },
    { id: "performance", title: "Performance", icon: "perf", key: "p" },
    { id: "oncall", title: "On-call agent", icon: "agent", key: "n", scope: "admin", route: "POST /v1/admin/oncall" },
    { id: "feedback", title: "Feedback", icon: "feedback", key: "f", scope: "write", route: "POST /v1/feedback" },
  ]],
  ["Run the cluster", [
    { id: "cluster", title: "Cluster", icon: "cluster", key: "c" },
    { id: "tables", title: "Tables and storage", icon: "layers", key: "o", fallback: "inspector" },
    { id: "profile", title: "Profiler", icon: "flame", key: "r" },
    { id: "querypath", title: "Query path", icon: "route", key: "x", cluster: true },
    { id: "rollout", title: "Rollouts", icon: "rollout", key: "u", scope: "admin", cluster: true },
    { id: "autoscale", title: "Autoscaling", icon: "autoscale", scope: "admin", cluster: true },
    { id: "simulator", title: "Simulator", icon: "gauge", scope: "admin", cluster: true },
    { id: "access", title: "Access and audit", icon: "access", key: "k", scope: "admin" },
    { id: "system", title: "Settings", icon: "system", key: "s", scope: "admin" },
    { id: "cloud", title: "Cloud setup", icon: "cloud", scope: "admin" },
    { id: "connections", title: "Connections", icon: "key", scope: "admin" },
    { id: "clusters", title: "All clusters", icon: "cluster", scope: "admin" },
    { id: "follow", title: "Follow", icon: "share", scope: "admin", cluster: true },
  ]],
  // Apps (--app name=url): one entry each, added by the shell once the
  // server lists them (views/apps.js); the group shows only when it has one.
  ["Apps", []],
  ["Labs", [
    { id: "relevance", title: "Relevance lab", icon: "relevance" },
    { id: "autopilot", title: "Relevance autopilot", icon: "relevance", scope: "admin" },
    { id: "recsys", title: "Recsys eval", icon: "recsys" },
    { id: "sid", title: "Semantic IDs", icon: "sid" },
    { id: "autocomplete", title: "Autocomplete", icon: "autocomplete" },
    { id: "understand", title: "Understanding", icon: "sparkle" },
    { id: "media", title: "Media search", icon: "media" },
    { id: "graph", title: "Graph explorer", icon: "graph" },
    // CON-1: the old console (#/classic) is out of the nav: its tools are
    // views here; the SQL workbench links it for its agent.
  ]],
];

/**
 * Old addresses and where they go now: [from, to, params mapper?]. A route
 * whose view does not exist (yet) falls back as NAV says (`fallback`).
 */
export const REDIRECTS = {
  overview: "home",
  topology: "cluster",
  fleet: "cluster",
  control: "cluster",
  settings: "system",
  storage: "tables",
  tablehome: "table",
  saved_searches: "saved",
  functions: "saved",
};
