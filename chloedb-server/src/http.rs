//! The HTTP JSON API, Prometheus metrics and the query UI.
//!
//! | route | body | answer |
//! |---|---|---|
//! | `GET /` | | the UI |
//! | `GET /livez`, `/readyz` | | `{status}`: 503 when a restart is needed; `/readyz` also while the server drains (`--drain-ms`) |
//! | `GET /v1/health`, `/v1/stats`, `/v1/config` | | JSON (`/v1/health`: 503 when "down" or "failing") |
//! | `GET /v1/schema?table=name` | | a table's schema (the first table's without `table`) |
//! | `GET /v1/tables` | | `{tables: [...]}`: every table's schema and rank profiles, the first first |
//! | `GET /v1/tables/:table/paths?field=attrs&path=dims&where=cond` | | `{table, field, rows, paths: [{path, keys, rows, types: {string, number, bool}, distinct, min?, max?, values?: [{value, rows}]}], truncated}`: the paths a JSON field's objects hold ([`chloedb::query::paths`]), under `path` if given, among the rows `where` matches; from a coordinator, every shard's summed |
//! | `GET /metrics` | | Prometheus text |
//! | `POST /v1/query` | `{sql, params?, timeout_ms?, shard?, stats?, snapshot?, min_seq_no?, min_seq_nos?, log?, trace?, steps?, profile?, format?, page_size?, keep_alive_ms?}`, or a scroll's next page: `{cursor, format?, keep_alive_ms?, timeout_ms?}` | rows, and with `steps` or `profile` in `stats.steps` the query's timed steps (with `profile`, their CPU time too, and with `trace` or `profile` the request's spans in `trace`); with `log` (`true` or `{table?, request?, context?, variant?, candidates?, ts?, interleave?: {sql, variants?}}`) also `request`, `logged` and, interleaved, `teams` ([`chloedb::rank_log`]); with `"format": "arrow"` the rows as an Arrow IPC stream (`application/vnd.apache.arrow.stream`, typed as `chloedb::query::arrow::answer_batch` types them; `x-strata-took-ms` and `x-strata-stats` headers); with `page_size` a scroll's first page (at most `--max-result-window` rows) and, while rows are left, its `cursor` (Arrow: the `x-strata-cursor` header), which reads the next page from the same version ([`crate::service`]'s scrolls) |
//! | `DELETE /v1/query/cursors/:id` | | `{closed}`: a scroll ended before its last page |
//! | `POST /v1/sql` | `{sql, params?, timeout_ms?, min_seq_no?, min_seq_nos?, log?}` | rows, or `{written, seq_no?, seq_nos?}` |
//! | `POST /v1/rank_log/export` | `{table?, since?, until?, events?, history?, window?, grades?}` | NDJSON: one record per logged row ([`chloedb::rank_log::export`]) |
//! | `POST /v1/rank_log/interleaving` | the export's body | `{pairs: [{variants, requests, acted, wins, ties, delta, p}], took_ms}`: which of two interleaved rankings the users preferred ([`chloedb::rank_log::Credit`]) |
//! | `POST /v1/write` | `{ops: [...]}` (see `proto/strata.proto`; each op writes the first table, or its `table`) | `{written, seq_no?, seq_nos?}` |
//! | `GET /v1/kv/:table/:key?min_seq_no=n` | | `{table, key, found, row?, seq_no?}`: the row with that primary key, read without SQL, as `SELECT *` answers it ([`Service::get_rows`]); 404 when no live row has it (or it has expired) |
//! | `POST /v1/kv/get` | `{keys, table?, columns?, timeout_ms?, min_seq_no?, min_seq_nos?}` | `{table, columns, rows, seq_nos, took_ms}`: per key, in their order, its row as an object (null for none) and its number, of `columns` alone when given; a coordinator asks each key's shard |
//! | `PUT /v1/kv/:table/:key?if_seq_no=n` | the row's fields as an object (the key may be left out) | `{written, seq_no?}`: the row written in place of any with the key (an upsert; with `if_seq_no`, only while the row's number is that) |
//! | `DELETE /v1/kv/:table/:key?if_seq_no=n&routing=v` | | `{deleted, written, seq_no?}` |
//! | `POST /v1/plan` | `{plan, timeout_ms?, min_seq_no?, min_seq_nos?}` (a plan as `/v1/explain` gives it) | rows, or `{written, seq_no?, seq_nos?}` |
//! | `POST /v1/import?format=jsonl&table=name` | rows as JSON Lines, JSON, CSV or TSV (of the first table without `table`) | `{written, skipped_fields}` |
//! | `POST /v1/import?format=sqlite&tables=a,b&plan=true` | a SQLite file (`application/vnd.sqlite3`; recognised by its first bytes too) | `{tables, skipped, written}`, or with `plan` the plan ([`chloedb::sqlite`]) |
//! | `POST /v1/import?format=parquet&table=name&key=id&plan=true` | a Parquet file (`application/vnd.apache.parquet`; recognised by its first bytes too) | `{table, rows, created, skipped, notes, written}`, or with `plan` the plan ([`chloedb::parquet`]) |
//! | `GET /v1/export?tables=a,b&fts=false&indexes=false&schemas=false` | | the database as a SQLite file |
//! | `POST /v1/admin/slices` | `{table?, text, lat, lon, lat0, lon0, tile_m?, k1?, b?}` (a geo slice), `{table?, vector, clusters?, seed?, codes?}` (a vector slice), or `{table}` alone to rebuild it as it was built | `{table, kind, version, rows, tiles, words or clusters and codes_bytes, manifest_bytes, tile_bytes, took_ms}`: builds a table's slice into `--slices` ([`chloedb::slice`]) |
//! | `GET /v1/admin/slices` | | `{slices: [...]}`: each slice, whether its table changed since, and how devices use it ([`crate::slices`]) |
//! | `GET /v1/admin/slices/:table`, `DELETE /v1/admin/slices/:table` | | one slice with each tile's rows, bytes and downloads, and the last hour by minute; or `{deleted}` |
//! | `GET /v1/slices/:table/manifest.json`, `GET /v1/slices/:table/tiles/<x>_<y>.jsonl.gz`, `GET /v1/slices/:table/codes.bin.gz` | | a slice's manifest, a tile's rows (gzipped JSON lines), and a vector slice's codes of every row, for a device to search near its user or its user's taste |
//! | `GET /v1/slices/:table/status` | | `{table, version, built_ms, fresh, changed_ms}`: whether a device's slice is the current one |
//! | `POST /v1/slices/:table/usage` | `{device?, searches, why: {proved, no_match, short, bound, stale}, device_ms?, cloud_ms?}` | `{}`: a device's searches since its last report |
//! | `GET /v1/export?format=parquet&tables=name` | | one table as a Parquet file, keeping its Strata schema |
//! | `POST /v1/schema/check` | a schema, in either JSON form | `{schema, short}`, or an error |
//! | `POST /v1/explain` | `{sql, params?, hits?}` | the bound query, its plan IR and the executor's plan; with `hits`, the top rows with their `score()` and `features()` |
//! | `POST /v1/embed` | `{text, field?, table?}` | `{table, field, vector}`: the text's vector by an embedded field's model; `vectors`, a vector a token, for a multi-vector field's (ColBERT) |
//! | `GET /v1/models` | | `{dir, models: [{name, dim, hosted, what, ready, error}]}`: the embedding models a schema can name, and whether each would open here |
//! | `GET /v1/media/<sha256>` | | the bytes of the file a row's MEDIA value names, by its hash, read as writes read files (`--media-store`, `--media-fetch`), served inert ([`media_headers`]); 404 when no row names it, 400 from a coordinator |
//! | `GET /v1/synonyms`, `PUT /v1/synonyms` | `{text}` (Solr format) | the server's synonym list; `GET` also lists each table's own (`tables`: `[{table, text, rules, version}]`, `CREATE SYNONYMS ON table`) |
//! | `POST /v1/synonyms/mine` | `{table, field, vector? \| log?, min_cos?, per_word?, max_words?, min_df?, window?, min_support?}` | `{proposals: [{from, to, rule, score, from_count, to_count}], rules, rows, words, took_ms}`: synonyms of a text field mined from its words' vectors or from a table of searches, proposed and not applied; on a server of its own |
//! | `POST /v1/term_stats` | `{sql, params?, timeout_ms?, min_seq_no?, min_seq_nos?}` | `{stats, snapshot}` |
//! | `GET /v1/classifiers` | | `{classifiers: [{name, version, method, table, field, labels_from, model, labels, label_names, holdout_accuracy, trained_rows, ...}]}`: the classifiers `CREATE CLASSIFIER` made, without their weights |
//! | `POST /v1/derive/preview` | `{model, text, table?, field?, many?}` | `{model, many, values}`: what a content model makes of a text, as a derived KEYWORD field would hold it: a built-in one (`builtin:lang`, `builtin:keyphrases`, `builtin:minhash`), a dictionary (`dict:<table>.<field>`, the text analysed as `table`'s `field`), or a classifier (`classifier:<name>`, its best label, or with `many` the labels a `KEYWORD[]` holds, and `top`) |
//! | `POST /v1/classify` | `{name, text, k?}` | `{label, score, top: [[label, probability]], classifier, version, table, field, labels_from, min_score}`: a text's classes by a classifier `CREATE CLASSIFIER` made, the text embedded by its field's model |
//! | `POST /v1/understand` | `{table, field, text, mode?, fields?, weight?}` | `{spans, ranges, ranked, filters, boosts, line, ...}`: the values of the table's keyword fields the words name, and ranges of numbers, as `bm25(.., understand => ..)` takes them |
//! | `POST /v1/correct` | `{table, field, text}` | `{text, changed, words: [{word, term, to, to_word, edits, chance}]}`: "did you mean", the words in no row taken for typos of the field's terms (one server or shard) |
//! | `POST /v1/snapshot` | | `{snapshot, txn, ttl_s}` |
//! | `POST /v1/agent` | `{question, model?, effort?, api_key?, max_steps?}` | an answer and the steps taken |
//! | `POST /v1/relevance/autopilot` | the autopilot's options, `judge?`, `keep?` | `{report, proposal, judged}`: logged queries replayed on one snapshot with the rank profile and with the numbers the search finds, measured on held-out queries ([`crate::autopilot`]) |
//! | `GET /v1/relevance/proposals`, `GET /v1/relevance/proposals/:id`, `POST .../:id/apply` `{force?}`, `POST .../:id/dismiss` | | the proposals kept, one with its proof, applied or dismissed |
//! | `POST /v1/feedback`, `GET /v1/feedback?status=&kind=&q=&limit=`, `GET`/`PATCH`/`DELETE /v1/feedback/:id`, `GET /v1/feedback/:id/shot` | `{note, kind?, shot?, marks?, ...}`; triage `{status?, priority?, assignee?, resolution?, if_status?}` | the command center's feedback, kept in table `cc_feedback` ([`crate::feedback`]): sent and triaged with a write key, read with a read key, deleted by an admin |
//! | `POST /v1/judge` | `{query, keys, table?, fields?, model?, api_key?}` | `{grades: [{key, grade, error?}], model, usage}`: each row graded 0 to 3 for the query by a Claude model ([`crate::judge`]) |
//! | `POST /v1/admin/oncall` | `{question?, alert?, follows?, model?, effort?, api_key?, max_steps?, note?, wait?}` | the run, `{run, answer, proposals, steps, said, usage, ...}`: a Claude model looks into an alert or a question with read-only tools and proposes actions ([`crate::oncall`]); `note` also writes the answer on the events timeline; `follows` asks a run's follow-up in its conversation; `wait: false` answers `{run, state: "running"}` at once |
//! | `GET /v1/admin/oncall` | | `{configured, model, steps, max_steps, runs, running, pending}`: whether the server has a Claude key, its defaults, and the runs kept, newest first, each in short |
//! | `GET /v1/admin/oncall/runs`, `GET /v1/admin/oncall/runs/:id` | | `{runs}` in short; one run in full, with its steps (and what each tool answered, cut), what the model wrote between them, its report and proposals, as it goes |
//! | `POST /v1/admin/oncall/runs/:id/stop` | | the run: it stops before its next model call |
//! | `GET /v1/admin/oncall/proposals?state=&run=` | | `{proposals}`: the on-call agent's proposed actions, newest first, each `pending`, `applied`, `failed`, `rejected` or `expired` (after an hour) |
//! | `POST /v1/admin/oncall/proposals/:id/approve`, `.../reject` | `{reason?}` to reject | the proposal: approving makes its request with the approver's credential and its reason, audited as theirs |
//! | `POST /v1/admin/changes/preview` | `{method, path, body?}` | the dry run of a change of any route: `{tier, would, reversible, undo, blast, affects, preflight, plan, plan_id, fingerprint}` ([`crate::changes`]) |
//! | `POST /v1/admin/changes`, `GET /v1/admin/changes?state=`, `GET /v1/admin/changes/:id` | `{method, path, body?, reason, what?}` to file | a change request, pending a person's approval for an hour; the list, `{changes, pending}`; one |
//! | `POST /v1/admin/changes/:id/approve`, `.../reject`, `.../withdraw` | `{reason?}` | approved by a person (an agent is refused `approval_refused`), dry run again first (`plan_stale` when it moved); rejected by a person; withdrawn by whoever filed it |
//! | `GET /v1/catalog?q=` | | every route with its scope, tier, roles, dry run and MCP tool, and every error code ([`crate::catalog`]) |
//! | `GET /v1/runbooks?for=&q=` | | the runbook for an alert or fleet problem, or by words ([`crate::runbooks`]) |
//! | `GET /v1/admin/jobs?kind=` | | `{jobs, running}`: plans, moves, on-call runs and change requests in one shape |
//! | `POST /v1/mcp` | a JSON-RPC message or a batch | the Model Context Protocol's answer, or 202 for notifications: tools that call these routes with the caller's credential ([`crate::mcp`]) |
//! | `GET /v1/queries` | | `{queries, running}`: the queries running now, the longest first, each with its trace so far; on a coordinator, every coordinator's (another's ids are `id@name`, with `local_id` and `nodes`: who answered) |
//! | `DELETE /v1/queries/:id` | | `{cancelled}`: the query stops at its next check and answers 499; an `id@name` is cancelled on that coordinator |
//! | `GET /v1/queries/recent?since=ms&limit=n`, `/v1/queries/slow?limit=n` | | `{queries}`: the latest that ended (or the slow ones), the newest first, each with its trace |
//! | `GET /v1/queries/request/:id` | | `{queries}`: those a request (its `x-request-id`) ran |
//! | `GET /v1/queries/finished/:id` | | one ended query with its whole profile: trace, steps ([`chloedb::query::steps`]) and plan lines (the lists give each without them, unless `full=true`) |
//! | `GET /v1/profile/settings`, `PUT /v1/profile/settings` | `{slow_ms?, cpu_all?}` | the slow threshold (until restarted) and whether every query's CPU time is measured; on a coordinator `?node=` a copy's, `?coordinator=` (URL or name) another coordinator's |
//! | `POST /v1/profile/cpu?format=folded&node=` | `{seconds?, hz?, sql?, params?, clients?}` | a CPU profile ([`crate::profiler`]) of the whole server for `seconds`, or of `sql` run again and again for that long by `clients` callers at once (1 to 64): every thread's stacks, sampled `hz` times a CPU second; `format=folded` answers folded lines (flamegraph.pl, speedscope); 409 while another is taken |
//! | `GET /v1/profile/memory?node=` | | the heap ([`crate::heap`]: bytes in use and their peak, bytes allocated, allocations and frees), the resident memory and its peak, and the threads |
//! | `POST /v1/profile/heap?format=folded&value=in_use&node=` | `{seconds?, every?, sql?, params?, clients?}` | an allocation profile ([`crate::heap::take`]): about one allocation in every `every` bytes (default 2 MiB) a thread allocates, sampled for `seconds`, with its stack; by stack, the bytes allocated and those still in use at the end; a query in a loop as for a CPU profile |
//! | `GET /v1/profile/threads?node=` | | each thread of the process: its name, state, CPU time and context switches (Linux); on a coordinator, each of these four with `node` (a replica URL of `/v1/cluster`) is that server's, asked over gRPC (`Profile`) |
//! | `GET /v1/queries/stats?from=&to=&limit=n`, `GET /v1/queries/stats/:fingerprint`, `POST /v1/queries/stats/reset` | | statistics per statement shape over a window of the last day, by time spent ([`crate::telemetry::Statements`]); on a coordinator every coordinator's, merged by shape, each saying its `coordinators`' calls (`local=1`: this one's; `raw=1`: the counts another coordinator merges) |
//! | `GET /v1/timeseries?since=ms` | | `{interval_ms, points}`: rates and gauges every few seconds, the last hour ([`crate::telemetry::Series`]) |
//! | `GET /v1/timeseries?from=&to=` | | the same over any range: memory's points when it holds `from`, else a point a minute from the log store's `load` (`source`) |
//! | `GET /v1/index/reads?table=` | | this server's read counts of a table (a coordinator's own, which the others merge into `/v1/index`'s `reads_from`) |
//! | `GET /v1/index/tables` | | every table's documents, bytes and segments, by shard, its skew, writes and reads a minute, growth over a day and a week, and copies behind; kept 30 s ([`crate::inspect::tables`]) |
//! | `POST /v1/admin/scrub`, `POST /v1/admin/copies/scrub` | `{rate_mib_s?}`, `{url}` | reads every segment of the database (of a copy, on a coordinator) against its checksum while writes go on, and answers `{scrub: {txn, segments, bytes, damaged, seconds}}`; 409 while one runs ([`crate::service::scrub`]) |
//! | `POST /v1/admin/copies/verify`, `POST /v1/admin/copies/checkpoint` | `{url, timeout_ms?}`, `{url, name, layout?, link_from?}` | on a coordinator, of a copy (its URL or short name): its shard's primary compares the replicas' rows with its own (`{replicas: [{url, in_sync, compared, differing_slots, first_differing_slot, seq_no, error}]}`), or the copy writes a checkpoint into its own `--backup-dir` (`{checkpoint}`) |
//! | `POST /v1/admin/plan` | `{shards?: [{shard, copies}], add_shards?, remove_shards?, moves?}` | on own machines, the steps of a change of the cluster's shape, rows to copy, an estimate, what clients feel and what to check first; changes nothing ([`crate::coord::plan`]) |
//! | `POST /v1/admin/plan/apply` | `{steps: [...], estimate_s?}`, or a plan's body | applies a plan on the coordinator, its steps in order, in the background; kept in a file so that a restart says where it stopped ([`crate::coord::plan::apply`]) |
//! | `GET /v1/admin/plan/run` | – | the plan applied or last applied here, step by step (also `plan` in `/v1/cluster`) |
//! | `POST /v1/admin/plan/run/stop`, `/resume`, `/discard` | – | stop after the step under way; apply a stopped plan again from its step; forget a stopped plan |
//! | `GET /v1/admin/kube/versions?limit=30` | | the images of the cluster's repository a person may pick, newest first, each `current`, `newer`, `tracked`; without Kubernetes too ([`crate::operator::upgrade::Versions`]) |
//! | `GET /v1/load?from=&to=&copies=all` | | on a coordinator, each copy's load a minute from its own log store; copies that did not answer under `missing` |
//! | `GET /v1/logs?after=seq&level=warn&request=id&q=text&limit=n&node=` | | `{lines, last, filter, filter_expires_ms, filter_set_by}`: the latest log lines ([`crate::obs::ring`]); on a coordinator with `node`, that copy's `{node, short, filter, filter_expires_ms, filter_set_by, filter_set_ms, previous}` (its lines are in the log store) |
//! | `GET /v1/logs/filter?node=`, `PUT /v1/logs/filter?node=` | `{filter, expires_s?}` (as `--log`) | `{filter, expires_ms, set_by, set_ms, previous}`: what is logged, for `expires_s` (30 min by default; 0: until set again) and then the filter before; on a coordinator, `node` (a copy's URL or short name) asks or sets that copy's, answered with `node` and `short`; a change is an event (`log_level`) |
//! | `POST /v1/logs/query` | `{sql, params?, timeout_ms?, scope?}` (SQL over tables `logs` and `queries`) | rows of the server's log store ([`crate::logstore`]); on a coordinator, of every server's merged, the other coordinators' stores too, with `nodes`: each server's rows, time and error (`"scope": "local"`: its own store alone; with `"partial": true`, in the form another coordinator merges) |
//! | `GET /v1/logs/store` | | `{store, schemas, nodes?}`: the log store (its path, rows, bytes, retention, what it wrote, dropped and deleted; null without one), its tables, and on a coordinator every server's |
//! | `GET /v1/system`, `/v1/cluster?brief=1`, `/v1/routes` | | the process and its limits; the cluster's copies (`brief=1`: without each copy's `detail`); requests by route and queries by kind |
//! | `GET /v1/coordinators?window=1` | | `{coordinators: [{url, name, this, last_seen_ms, version}], gone, live_ms}`: this cluster's coordinators, from what the shards heard ([`crate::peers`]); `window=1` adds this one's last minute and 5 minutes of searches |
//! | `GET /v1/whoami` | | `{name, key, scope, root, cluster, caller}`: who the request's credential is ([`crate::keys`]) |
//! | `GET /v1/events?from=&to=&kind=&limit=`, `POST /v1/events/notes` | `{text, ts_ms?, where?}` | what happened, the newest first: copies down and back, new primaries, changes, notes ([`crate::events`]) |
//! | `GET /v1/health/fleet` | | `{status, headline, since_ms, checks, problems, coordinators}`: whether the cluster is all right and what is wrong first ([`crate::fleet`]) |
//! | `GET /v1/admin/model-budget` | | the model calls each API key made today with the server's credentials, by key id, and how many it may (`per_day`, [`crate::budget`]) |
//! | `GET /v1/admin/keys`, `POST /v1/admin/keys`, `DELETE /v1/admin/keys/:id`, `POST /v1/admin/keys/:id/rotate`, `POST /v1/admin/keys/:id/budget` | `{name, scope, expires_days?, owner?, note?, budget?}`, rotate `{grace_minutes?}`, budget `{nodes?, depth?, list?, plan_ms?, rows_read?}` (lowers it) | the API keys, how many were revoked, and how the last merge of keys went with each server of the cluster (`peers`, [`crate::keysync`]); a new one's secret is answered once; each key with its owner, note and last address, and the revoked ones (who revoked them, when); a rotation answers a new key of the same name and scope, the old one expiring after the grace (60 min) |
//! | `GET /v1/admin/keys/clients?hours=24` | | which clients sent each key over the last `hours` (1 to 168): by key id, each client's app, client, address, calls, failed calls, first and last; every query counts, not a sample; on a coordinator every coordinator's summed, each client naming the `coordinators` it went through (`coordinators` says which answered; `local=1` for this one's); `since_ms`: from when every coordinator counted |
//! | `GET /v1/admin/audit?kind=&from=&to=&who=&q=&limit=n` | | the changes, notes and refused requests, the newest first, with `kind` (change, note, refusal), who, addr, client, reason, object, before and after; older than memory holds from the log store's `audit`; on a coordinator every coordinator's, each entry naming its `coordinator` (`coordinators` says which answered; `local=1` for this one's) |
//! | `GET /v1/alerts`, `PUT /v1/alerts/rules` | `{rules}` | the alert rules with their states and the latest changes ([`crate::alerts`]); `brief=1`: each rule's state, and of those not ok what the bell shows; on a coordinator the PUT answers how many copies took them (`pushed`) |
//! | `GET`/`PUT /v1/alerts/routes`, `POST /v1/alerts/routes/:name/test` | `{routes: [{name, kind, url, key, severities, repeat_min}]}` | where notifications go, each with its deliveries (sent, failed, last error); a test message (admin) |
//! | `POST /v1/alerts/backtest` | `{rule, from, to}` | the rule replayed over memory's points, up to 7 days of the log store's load, the coordinator's events or each copy's own points: `{points, fired, source, covered_from}` |
//! | `GET`/`POST /v1/alerts/peer` | what a coordinator holds | coordinators tell each other their rules, routes, acknowledgements, silences, notes and pages (admin, with `x-strata-peer`) |
//! | `GET /v1/alerts/:rule`, `POST /v1/alerts/:rule/ack`, `POST`/`DELETE /v1/alerts/:rule/silence`, `POST /v1/alerts/:rule/notes` | `{note}`, `{minutes, reason}`, `{text}` | one alert with its events (7 days), acknowledged, silenced, noted (write scope) |
//!
//! Every answer carries the request's id in `x-request-id`: the one the
//! client sent, if it is 1 to 64 letters, digits, '-', '_', '.' or ':', or
//! a new one ([`crate::obs`]).
//!
//! With a token or a cluster token set, or an API key made, every route but
//! `/`, `/livez`, `/readyz` and `/v1/health` needs `Authorization: Bearer
//! <token or key>`, and a key the scope of its route ([`route_scope`]). Every route checks the `Host` header
//! and, on a request that can change something, the `Origin` header
//! ([`crate::auth`]).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use futures::StreamExt;
use axum::extract::{DefaultBodyLimit, MatchedPath, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json as AxJson, Router};
use serde_json::{json, Value as Json};
use tracing::Instrument;

use chloedb::ingest::Format;
use chloedb::storage::Layout;

use crate::auth::{self, Access, Secret};
use crate::replication::Acks;
use crate::service::{has_source, is_mark, After, QueryReq, Service, SqlAnswer, Stream, Written, STREAM_OPS};
use crate::keys::{self, Principal, Scope};
use crate::{agent, json as js, judge, obs, profiling, telemetry, ui, ApiError};


/// Why a request failed on the server's side, for [`observe`] to log.
#[derive(Clone)]
struct Failure(Arc<str>);

impl IntoResponse for ApiError {
    fn into_response(mut self) -> Response {
        // A key the table holds already (the engine's `conflict`,
        // [`chloedb::Error::code`]): 409 over HTTP. Not over gRPC, where a
        // coordinator takes FAILED_PRECONDITION for a slot map's refusal.
        // So is a strict write's refusal, 400 over gRPC for the same reason.
        if self.status == 400 && (self.message.contains("duplicate primary key") || self.message.contains("strict: ")) {
            self.status = 409;
        }
        let code = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let failure = (self.status >= 500).then(|| Failure(self.message.as_str().into()));
        let client = (400..500).contains(&self.status).then(|| ClientError(self.message.as_str().into()));
        let mut r = (code, AxJson(crate::errors::answer(&self))).into_response();
        if let Some(c) = client {
            r.extensions_mut().insert(c);
        }
        // Refused for the server's load, or while it holds its writes: when
        // to try again.
        if self.status == 429 || self.status == 503 {
            r.headers_mut().insert(header::RETRY_AFTER, header::HeaderValue::from(crate::service::RETRY_AFTER.as_secs()));
        }
        if let Some(f) = failure {
            r.extensions_mut().insert(f);
        }
        r
    }
}

type Res = Result<AxJson<Json>, ApiError>;

#[derive(Clone)]
struct App {
    svc: Arc<Service>,
    token: Option<Secret>,
    cluster: Option<Secret>,
    hosts: Option<Arc<[String]>>,
    /// The API's routes again, which `/v1/mcp`'s tools call ([`crate::mcp`]):
    /// none in that copy itself.
    mcp: Option<Router>,
}

impl App {
    /// Whether callers must prove themselves by more than a key: a token or
    /// the cluster's.
    fn guarded(&self) -> bool {
        self.token.is_some() || self.cluster.is_some()
    }
}

pub fn router(svc: Arc<Service>, access: &Access) -> Router {
    let app = App { svc, token: access.token.clone(), cluster: access.cluster.clone(), hosts: access.hosts.clone().map(Into::into), mcp: None };
    // alerts2: coordinators tell each other of their alerts.
    crate::alerts::peering(&app.svc);
    // cluster2: a plan a restart stopped is taken up (interrupted).
    crate::coord::plan::start(&app.svc);
    // mcp: the routes an MCP tool's call goes to, with the caller's
    // credential, through the same checks (a copy, so the router does not
    // hold itself); and health, which takes no credential (the on-call
    // agent's first look, [`crate::oncall`]).
    // Two copies deep: a dry run or change request a tool files makes
    // requests of its own (a plan, a count of rows), through the copy
    // below it, which files none.
    let copy = |app: &App| api(app).route("/v1/health", get(health)).fallback(no_route).layer(tower::ServiceBuilder::new().layer(middleware::from_fn_with_state(app.clone(), observe)).layer(DefaultBodyLimit::max(crate::service::SMALL_BODY))).with_state(app.clone());
    let inner = copy(&app);
    let tools = copy(&App { mcp: Some(inner), ..app.clone() });
    // connectors: their runs write through the same routes and checks.
    crate::connectors::Connectors::start(&app.svc, tools.clone(), app.hosts.as_deref().and_then(|h| h.first().cloned()));
    let app = App { mcp: Some(tools), ..app };
    let api = api(&app);
    Router::new()
        .route("/", get(ui::index))
        .route("/classic", get(ui::classic))
        .route("/ui/*file", get(ui::file))
        .route("/v1/health", get(health))
        .route("/livez", get(live))
        .route("/readyz", get(ready))
        // The command center of a connection of the hub's book: the same
        // page, whose calls go to /c/<name>/v1/...
        .route("/c/:name", get(ui::index))
        .merge(api)
        .fallback(no_route)
        // One stack, outermost first, put on each route once: each layer
        // put on its own wraps the routes in another boxed service, and
        // each request then clones every box inside the layer it passes
        // (some 40 allocations a request, with five layers).
        .layer(tower::ServiceBuilder::new().layer(middleware::from_fn_with_state(app.clone(), observe)).layer(DefaultBodyLimit::max(crate::service::SMALL_BODY)).layer(compression()))
        .with_state(app)
}

/// A path no route matches: the API's error, not an empty 404.
async fn no_route(method: axum::http::Method, uri: axum::http::Uri) -> ApiError {
    ApiError::coded(404, "not_found", format!("no route matches {method} {} (GET /v1/catalog lists the API's routes)", uri.path()))
}

/// An error answered before a handler ran, by axum (a body that is not
/// JSON, no JSON content type, a body over its limit, a method the route
/// does not take): plain text or nothing, made the API's error shape
/// ([`crate::errors`]) with its sentence, so a client reads every error
/// alike. Other answers are left as they are.
async fn json_error(res: Response, max_body: usize) -> Response {
    let status = res.status().as_u16();
    if !matches!(status, 400 | 405 | 411 | 413 | 415 | 422) {
        return res;
    }
    let ct = res.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !(ct.is_empty() || ct.starts_with("text/plain")) {
        return res;
    }
    let allow = res.headers().get(header::ALLOW).cloned();
    let (parts, body) = res.into_parts();
    let text = match axum::body::to_bytes(body, 64 << 10).await {
        Ok(b) => String::from_utf8_lossy(&b).trim().to_string(),
        Err(_) => String::new(),
    };
    let message = match status {
        405 => format!("the route does not take this method{}", allow.as_ref().and_then(|a| a.to_str().ok()).map_or(String::new(), |a| format!(": it takes {a}"))),
        413 => format!(
            "the request's body is larger than the route takes: {} MiB for rows (/v1/write, /v1/import and the like; --max-body-mb), {} MiB for any other route",
            max_body >> 20,
            crate::service::SMALL_BODY >> 20
        ),
        _ if text.is_empty() => parts.status.canonical_reason().unwrap_or("refused").to_string(),
        _ => text,
    };
    let mut r = ApiError { code: None, status, message }.into_response();
    if let Some(a) = allow {
        r.headers_mut().insert(header::ALLOW, a);
    }
    r
}

/// The API's routes, behind [`auth`].
fn api(app: &App) -> Router<App> {
    // Rows come in writes and imports; every other body is small.
    let rows = DefaultBodyLimit::max(app.svc.max_body);
    Router::new()
        .route("/v1/stats", get(stats))
        .route("/v1/schema", get(schema))
        .route("/v1/tables", get(tables))
        .route("/v1/functions", get(functions))
        .route("/v1/tables/:table/paths", get(json_paths))
        .route("/v1/index", get(index))
        .route("/v1/index/reads/reset", post(reset_reads))
        // fleet: one coordinator's reads, for the others to merge (INS-3).
        .route("/v1/index/reads", get(index_reads))
        .route("/v1/config", get(config))
        .route("/metrics", get(metrics))
        .route("/v1/query", post(query))
        .route("/v1/query/cursors/:id", axum::routing::delete(close_cursor))
        .route("/v1/sql", post(sql))
        .route("/v1/write", post(write).layer(rows.clone()))
        .route("/v1/kv/get", post(kv_get))
        .route("/v1/kv/:table/:key", get(kv_one).merge(put(kv_put)).merge(axum::routing::delete(kv_delete)).layer(rows.clone()))
        .route("/v1/plan", post(run_plan).layer(rows.clone()))
        .route("/v1/import", post(import).layer(rows.clone()))
        .route("/v1/export", get(export))
        // A stream's body has no end set.
        .route("/v1/stream", post(stream).layer(DefaultBodyLimit::disable()))
        .route("/v1/sources", get(sources))
        .route("/v1/feed", get(feed))
        .route("/v1/feed/promoted", get(feed_promoted))
        .route("/v1/feed/keys", post(feed_keys))
        .route("/v1/sources/*name", get(source))
        .route("/v1/schema/check", post(schema_check))
        .route("/v1/sql/check", post(sql_check))
        .route("/v1/explain", post(explain))
        .route("/v1/embed", post(embed))
        .route("/v1/models", get(models))
        // The command center's model manager: an admin's (route_scope).
        .route("/v1/admin/models/fetch", post(fetch_model))
        .route("/v1/admin/models/convert", post(convert_model))
        .route("/v1/media/:sha256", get(media_file))
        .route("/v1/synonyms", get(synonyms).merge(put(put_synonyms)))
        .route("/v1/synonyms/mine", post(mine_synonyms))
        .route("/v1/term_stats", post(term_stats))
        .route("/v1/correct", post(correct))
        .route("/v1/understand", post(understand))
        .route("/v1/classify", post(classify))
        .route("/v1/classifiers", get(classifiers))
        .route("/v1/derive/preview", post(derive_preview))
        .route("/v1/sid/next", post(sid_next))
        .route("/v1/rank_log/export", post(export_rank_log))
        .route("/v1/rank_log/interleaving", post(interleaving))
        .route("/v1/snapshot", post(snapshot))
        .route("/v1/admin/checkpoint", post(admin_checkpoint))
        .route("/v1/admin/slices", get(admin_slices_list).post(admin_slices))
        .route("/v1/admin/slices/:table", get(admin_slice).delete(admin_slice_delete))
        .route("/v1/slices/:table/manifest.json", get(slice_manifest))
        .route("/v1/slices/:table/tiles/:file", get(slice_tile))
        .route("/v1/slices/:table/codes.bin.gz", get(slice_codes))
        .route("/v1/slices/:table/status", get(slice_status))
        .route("/v1/slices/:table/usage", post(slice_usage))
        .route("/v1/admin/check", post(admin_check))
        .route("/v1/admin/scrub", post(admin_scrub))
        .route("/v1/admin/vacuum", post(admin_vacuum))
        .route("/v1/admin/verify", post(admin_verify))
        .route("/v1/admin/promote", post(admin_promote))
        .route("/v1/admin/follow", get(follow_view))
        .route("/v1/admin/follow/status", post(follow_status))
        .route("/v1/admin/follow/stop", post(follow_stop))
        .route("/v1/admin/follow/pause", post(follow_pause))
        .route("/v1/admin/follow/promote", post(follow_promote))
        .route("/v1/admin/hold-writes", get(hold_view).post(hold_writes))
        .route("/v1/admin/shards", post(add_shard))
        .route("/v1/admin/shards/:i", axum::routing::delete(remove_shard))
        .route("/v1/admin/shards/:i/replicas", post(add_replica).merge(axum::routing::delete(remove_replica)))
        .route("/v1/admin/shards/:i/drain", post(drain_shard))
        .route("/v1/admin/shards/:i/promote", post(promote_in_shard))
        .route("/v1/admin/rebalance", post(rebalance))
        .route("/v1/admin/moves", get(moves).merge(post(move_slots)))
        .route("/v1/admin/moves/stop", post(stop_moves))
        .route("/v1/admin/moves/finish", post(finish_move))
        .route("/v1/agent", post(agent_run))
        .route("/v1/judge", post(judge))
        .route("/v1/relevance/autopilot", post(autopilot_run))
        .route("/v1/relevance/proposals", get(autopilot_list))
        .route("/v1/relevance/proposals/:id", get(autopilot_get))
        .route("/v1/relevance/proposals/:id/apply", post(autopilot_apply))
        .route("/v1/relevance/proposals/:id/dismiss", post(autopilot_dismiss))
        // The command center's feedback ([`crate::feedback`]).
        .route("/v1/feedback", get(feedback_list).post(feedback_submit))
        .route("/v1/feedback/:id", get(feedback_get).patch(feedback_update).delete(feedback_delete))
        .route("/v1/feedback/:id/shot", get(feedback_shot))
        .route("/v1/feedback/:id/thumb", get(feedback_thumb))
        .route("/v1/admin/oncall", get(oncall_overview).post(oncall_run))
        .route("/v1/admin/oncall/runs", get(oncall_runs))
        .route("/v1/admin/oncall/runs/:id", get(oncall_run_get))
        .route("/v1/admin/oncall/runs/:id/stop", post(oncall_stop))
        .route("/v1/admin/oncall/proposals", get(oncall_proposals))
        .route("/v1/admin/oncall/proposals/:id/approve", post(oncall_approve))
        .route("/v1/admin/oncall/proposals/:id/reject", post(oncall_reject))
        // Dry runs and change requests ([`crate::changes`]), the catalog of
        // routes ([`crate::catalog`]) and the jobs running.
        .route("/v1/admin/changes", get(changes_list).post(changes_file))
        .route("/v1/admin/changes/preview", post(changes_preview))
        .route("/v1/admin/changes/:id", get(changes_get))
        .route("/v1/admin/changes/:id/approve", post(changes_approve))
        .route("/v1/admin/changes/:id/reject", post(changes_reject))
        .route("/v1/admin/changes/:id/withdraw", post(changes_withdraw))
        .route("/v1/catalog", get(catalog))
        .route("/v1/runbooks", get(runbooks))
        .route("/v1/admin/jobs", get(jobs))
        .route("/v1/mcp", post(mcp).layer(rows))
        // workload (alerts agent): grouped, and cancelled by statement
        .route("/v1/queries", get(live_queries).delete(cancel_queries))
        .route("/v1/admin/contain", get(contain_rules).post(add_contain))
        .route("/v1/admin/contain/:id", axum::routing::delete(remove_contain))
        .route("/v1/queries/:id", axum::routing::delete(cancel_query))
        .route("/v1/queries/recent", get(recent_queries))
        .route("/v1/queries/slow", get(slow_queries))
        .route("/v1/queries/request/:id", get(request_queries))
        .route("/v1/queries/finished/:id", get(finished_query))
        .route("/v1/profile/settings", get(profile_settings).merge(put(put_profile_settings)))
        .route("/v1/profile/cpu", post(cpu_profile))
        .route("/v1/profile/memory", get(memory))
        .route("/v1/profile/heap", post(heap_profile))
        .route("/v1/profile/threads", get(threads))
        .route("/v1/queries/stats", get(query_stats))
        .route("/v1/queries/stats/reset", post(reset_query_stats))
        // workload (alerts agent): one statement over a window
        .route("/v1/queries/stats/:fingerprint", get(query_stat))
        .route("/v1/timeseries", get(timeseries))
        // otlp: OpenTelemetry logs in (POST), beside the log store's own lines (GET).
        .route("/v1/logs", get(logs).merge(post(otlp_logs).layer(rows.clone())))
        .route("/v1/logs/filter", put(log_filter).merge(get(log_filter_now)))
        .route("/v1/logs/query", post(log_query))
        .route("/v1/logs/store", get(log_store))
        .route("/v1/system", get(system))
        .route("/v1/cluster", get(cluster))
        // Apps: their list, and requests forwarded to them (crate::apps).
        .route("/v1/apps", get(apps_list))
        // An app may take files (a spreadsheet's import): rows' size, as /v1/import.
        .route("/v1/apps/:name/*rest", axum::routing::any(app_forward).layer(rows.clone()))
        // Tenants (experimental): databases kept in an object store (crate::tenants).
        .route("/v1/admin/tenants", get(tenant_list).post(tenant_create))
        .route("/v1/admin/tenants/:id", get(tenant_get).delete(tenant_delete))
        .route("/v1/admin/tenants/:id/sql", post(tenant_sql).layer(rows.clone()))
        .route("/v1/admin/tenants/:id/base", post(tenant_base))
        // A blackboard's tools, for agents (crate::board).
        .route("/v1/admin/tenants/:id/board/:tool", post(tenant_board))
        .route("/v1/admin/tenants/:id/mcp", post(tenant_mcp))
        .route("/v1/kube", get(kube))
        .route("/v1/admin/kube", post(kube_change))
        .route("/v1/admin/kube/plan", post(kube_plan))
        .route("/v1/admin/kube/resume", post(kube_resume))
        .route("/v1/admin/kube/upgrade", post(kube_upgrade))
        .route("/v1/admin/kube/autoscale/dismiss", post(autoscale_dismiss))
        .route("/v1/admin/autoscale/replay", post(autoscale_replay))
        .route("/v1/admin/aws", get(aws_status))
        .route("/v1/admin/aws/credentials", put(aws_save).merge(axum::routing::delete(aws_forget)))
        .route("/v1/admin/aws/regions", get(aws_regions))
        .route("/v1/admin/aws/machines", get(aws_machines))
        .route("/v1/admin/aws/plan", post(aws_plan))
        .route("/v1/admin/aws/validate", post(aws_validate))
        .route("/v1/admin/digitalocean", get(do_status))
        .route("/v1/admin/digitalocean/token", put(do_save).merge(axum::routing::delete(do_forget)))
        .route("/v1/admin/digitalocean/regions", get(do_regions))
        .route("/v1/admin/digitalocean/machines", get(do_machines))
        .route("/v1/admin/digitalocean/plan", post(do_plan))
        .route("/v1/admin/digitalocean/validate", post(do_validate))
        // The hub: connections to other clusters, and calls forwarded to them.
        .route("/v1/admin/hub", get(hub_view))
        .route("/v1/admin/hub/connections", post(hub_add))
        .route("/v1/admin/hub/connections/:name", post(hub_update).delete(hub_remove))
        .route("/v1/admin/hub/connections/:name/check", post(hub_check))
        .route("/v1/admin/hub/adopt", post(hub_adopt))
        .route("/v1/admin/hub/copy", post(hub_copy))
        .route("/v1/admin/hub/follow", post(hub_follow))
        .route("/c/:name/*rest", axum::routing::any(hub_forward))
        .route("/v1/admin/cloud/clusters", get(cloud_list).post(cloud_create))
        .route("/v1/admin/cloud/clusters/:id", get(cloud_one).merge(axum::routing::delete(cloud_forget)))
        .route("/v1/admin/cloud/clusters/:id/delete", post(cloud_delete))
        .route("/v1/admin/cloud/clusters/:id/expiry", post(cloud_expiry))
        .route("/v1/admin/cloud/found", get(cloud_found))
        .route("/v1/admin/cloud/found/delete", post(cloud_found_delete))
        // connectors: configs, runs and health (crate::connectors).
        .route("/v1/admin/connectors", get(connectors_list).post(connectors_create))
        .route("/v1/admin/connectors/test", post(connectors_test))
        .route("/v1/admin/connectors/:name", get(connectors_one).post(connectors_update).delete(connectors_delete))
        .route("/v1/admin/connectors/:name/run", post(connectors_run))
        .route("/v1/admin/connectors/:name/key", post(connectors_rotate))
        .route("/v1/connectors/:name/push", post(connectors_push).layer(DefaultBodyLimit::max(app.svc.max_body.min(crate::connectors::PUSH_BODY_MAX))))
        .route("/v1/routes", get(routes))
        .route("/v1/whoami", get(whoami))
        .route("/v1/admin/keys", get(list_keys).merge(post(create_key)))
        .route("/v1/admin/model-budget", get(model_budget))
        .route("/v1/admin/keys/clients", get(key_clients))
        .route("/v1/admin/keys/:id", axum::routing::delete(revoke_key))
        // A key's own keys, within its tables (an app engine's for each app).
        .route("/v1/keys/own", get(own_keys).merge(post(create_own_key)))
        .route("/v1/keys/own/:id", axum::routing::delete(revoke_own_key))
        .route("/v1/admin/audit", get(audit_log))
        // alerts (alerts agent)
        .route("/v1/alerts", get(alerts))
        .route("/v1/alerts/rules", put(put_alert_rules))
        .route("/v1/alerts/routes", get(alert_routes).put(put_alert_routes))
        .route("/v1/alerts/routes/:name/test", post(test_alert_route))
        .route("/v1/alerts/backtest", post(alert_backtest))
        // alerts2: what another coordinator holds and tells of the alerts
        .route("/v1/alerts/peer", get(alert_peer_state).post(alert_peer_merge))
        .route("/v1/alerts/:rule", get(alert))
        .route("/v1/alerts/:rule/ack", post(ack_alert))
        .route("/v1/alerts/:rule/silence", post(silence_alert).delete(unsilence_alert))
        .route("/v1/alerts/:rule/notes", post(note_alert))
        // ----- core: the rebuilt command center's contract (fleet health,
        // events, time beyond an hour, versions, plans, tables)
        .route("/v1/health/fleet", get(fleet_health))
        // fleet: the coordinators of this cluster ([`crate::peers`]).
        .route("/v1/coordinators", get(coordinators))
        .route("/v1/events", get(events_list))
        .route("/v1/events/notes", post(events_note))
        .route("/v1/admin/keys/:id/rotate", post(rotate_key))
        .route("/v1/admin/keys/:id/budget", post(lower_key_budget))
        .route("/v1/load", get(copies_load))
        .route("/v1/admin/kube/versions", get(kube_versions))
        .route("/v1/admin/plan", post(admin_plan))
        // cluster2: a plan applied on the coordinator, as a job (coord/plan.rs).
        .route("/v1/admin/plan/apply", post(plan_apply))
        .route("/v1/admin/plan/run", get(plan_run))
        .route("/v1/admin/plan/run/stop", post(plan_stop))
        .route("/v1/admin/plan/run/resume", post(plan_resume))
        .route("/v1/admin/plan/run/discard", post(plan_discard))
        .route("/v1/index/tables", get(index_tables))
        .route("/v1/admin/copies/verify", post(copy_verify))
        .route("/v1/admin/copies/checkpoint", post(copy_checkpoint))
        .route("/v1/admin/copies/scrub", post(copy_scrub))
        // The Simulator: a test run of the traffic simulator's mix against
        // this cluster, graded, with a model of other shapes (src/traffic).
        .route("/v1/admin/simulator", get(sim_view).post(sim_start))
        .route("/v1/admin/simulator/stop", post(sim_stop))
        .route("/v1/admin/simulator/predict", post(sim_predict))
        .route("/v1/admin/simulator/runs/:id", get(sim_report))
        .route("/v1/admin/simulator/table", axum::routing::delete(sim_drop))
        // ----- end core
        .route_layer(middleware::from_fn_with_state(app.clone(), auth))
}

/// gzip for a client that asks for it (`Accept-Encoding: gzip`), of JSON
/// answers and the UI's pages, scripts and styles (SH-11: a full
/// `/v1/cluster` is about 5.5 KB a copy, a tenth of that compressed).
/// Not of event streams, NDJSON, downloads or answers under 1 KB.
fn compression() -> tower_http::compression::CompressionLayer<impl tower_http::compression::Predicate + Clone> {
    use tower_http::compression::predicate::{Predicate, SizeAbove};
    let text = |_: axum::http::StatusCode, _: axum::http::Version, h: &HeaderMap, _: &axum::http::Extensions| {
        h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| ["application/json", "text/javascript", "application/javascript", "text/css", "text/html"].iter().any(|k| t.starts_with(k)))
    };
    tower_http::compression::CompressionLayer::new().gzip(true).compress_when(SizeAbove::new(1024).and(text))
}

/// Gives the request its id (answered in `x-request-id`), serves it in a
/// span that names it, counts it by route and status, and logs it: at
/// debug level, or as an error when it failed on the server's side. The
/// log viewer's own requests (`/v1/logs...`) are not logged as answered:
/// a viewer following the logs would otherwise read lines of its reading.
///
/// It also keeps out other sites' pages ([`browsers`]), and while the
/// server drains, closes each answer's connection ([`close_when_draining`]).
/// One layer rather than three: each layer costs a request a boxed future
/// and clones of the services inside it.
async fn observe(State(app): State<App>, req: Request, next: Next) -> Response {
    let id = obs::request_id(header_str(req.headers(), header::HeaderName::from_static(obs::HEADER)));
    // The route's name is shared, not copied.
    let matched = req.extensions().get::<MatchedPath>().cloned();
    let route = matched.as_ref().map_or("other", |p| p.as_str());
    let span = tracing::info_span!("request", id = %id, method = %req.method(), route = %route);
    let t0 = Instant::now();
    let peer = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0);
    let caller = caller_of(req.headers(), peer, &app.svc.trusted_proxies);
    let ui = caller.command_center();
    let (res, deadline) = match browsers(&app, &req) {
        Some(refused) => (refused, None),
        None => {
            let inner = keys::calling(caller, obs::scope(Some(id.clone()), next.run(req)));
            // A combinator, not an `async move` block, which would keep `inner` twice.
            let run = crate::service::DEADLINE.scope(crate::service::Deadline::default(), futures::FutureExt::map(inner, |res| (res, crate::service::deadline_of_request())));
            telemetry::traced(telemetry::Trace::new(), run).instrument(span.clone()).await
        }
    };
    let mut res = if res.status().as_u16() >= 400 { json_error(res, app.svc.max_body).await } else { res };
    let secs = t0.elapsed().as_secs_f64();
    let status = res.status().as_u16();
    // The command center's own requests are not the clients' traffic
    // (`/v1/routes`, the series' HTTP rates).
    if !ui {
        app.svc.metrics.http(route, status, secs);
    }
    if let Ok(v) = header::HeaderValue::from_str(&id) {
        res.headers_mut().insert(obs::HEADER, v);
    }
    close_when_draining(&mut res);
    let _in = span.enter();
    match res.extensions().get::<Failure>() {
        // The client's own deadline passed: its choice, not a fault.
        Some(Failure(why)) if deadline.is_some() => {
            static LIMIT: obs::RateLimit = obs::RateLimit::new(10);
            if let Some(held) = LIMIT.admit() {
                let ms = deadline.unwrap_or(0);
                tracing::warn!(status, deadline_ms = ms, error = %why, held_back = held, "client deadline of {ms} ms passed");
            }
        }
        Some(Failure(why)) => obs::server_error(status, why),
        None if route.starts_with("/v1/logs") => {}
        None => tracing::debug!(status, secs, "answered"),
    }
    res
}

fn header_str<'a>(headers: &'a HeaderMap, name: header::HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

// ----- core: who is calling (command center rebuild)

/// A network as `--trust-proxy` names it: an address, or an address and
/// a prefix length (`10.0.0.0/8`, `::1`).
pub fn parse_net(s: &str) -> Result<(std::net::IpAddr, u8), String> {
    let (ip, bits) = match s.trim().split_once('/') {
        Some((ip, b)) => (ip, Some(b)),
        None => (s.trim(), None),
    };
    let ip: std::net::IpAddr = ip.parse().map_err(|_| format!("{s:?} is not an address or a network (10.0.0.0/8)"))?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let bits = match bits {
        Some(b) => b.parse::<u8>().ok().filter(|b| *b <= max).ok_or_else(|| format!("{s:?}: the prefix length is 0 to {max}"))?,
        None => max,
    };
    Ok((ip, bits))
}

/// Whether `ip` is in network `net`.
fn in_net(ip: std::net::IpAddr, net: (std::net::IpAddr, u8)) -> bool {
    use std::net::IpAddr;
    let (bits, a, b) = match (ip, net.0) {
        (IpAddr::V4(a), IpAddr::V4(b)) => (net.1 as u32, u32::from(a) as u128, u32::from(b) as u128),
        (IpAddr::V6(a), IpAddr::V6(b)) => (net.1 as u32, u128::from(a), u128::from(b)),
        _ => return false,
    };
    let width = if ip.is_ipv4() { 32 } else { 128 };
    if bits == 0 {
        return true;
    }
    let shift = width - bits.min(width);
    (a >> shift) == (b >> shift)
}

/// A header's value as text, `%XX` escapes decoded (the command center
/// encodes a reason that is not ASCII).
fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(name)?.to_str().ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    let b = raw.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(x) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(x);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    Some(String::from_utf8_lossy(&out).chars().take(1000).collect())
}

/// Who a request is from ([`keys::Caller`]): its address (the peer, or
/// the first of `X-Forwarded-For` when the peer is a proxy in `trusted`),
/// its client (`x-strata-client`), its app (`x-strata-app`, else the
/// product of its `User-Agent`) and its reason (`x-strata-reason`).
pub fn caller_of(headers: &HeaderMap, peer: Option<std::net::SocketAddr>, trusted: &[(std::net::IpAddr, u8)]) -> keys::Caller {
    let mut addr = peer.map(|p| p.ip());
    if let Some(p) = addr.filter(|ip| trusted.iter().any(|n| in_net(*ip, *n))) {
        let forwarded = header_str(headers, header::HeaderName::from_static("x-forwarded-for")).and_then(|v| v.split(',').next()).and_then(|v| v.trim().parse().ok());
        addr = Some(forwarded.unwrap_or(p));
    }
    let client = header_text(headers, "x-strata-client").map(|c| c.to_ascii_lowercase()).filter(|c| c == keys::COMMAND_CENTER);
    let app = header_text(headers, "x-strata-app").or_else(|| header_str(headers, header::USER_AGENT).and_then(keys::product));
    let short = |n: &str| header_text(headers, n).map(|v| v.chars().take(120).collect::<String>());
    keys::Caller {
        addr: addr.map(|a| a.to_string()),
        client,
        app,
        reason: header_text(headers, "x-strata-reason"),
        agent_model: short("x-strata-agent-model"),
        agent_session: short("x-strata-agent-session"),
        agent_run: short("x-strata-agent-run"),
        change: short("x-strata-change"),
    }
}

/// A copy of a coordinator's cluster, by its URL or short name (`s1-0`):
/// its shard, URL, short name, and its shard's primary.
struct CopyOf {
    shard: u64,
    url: String,
    short: String,
    primary: Option<String>,
}

fn copy_of(snap: &Json, node: &str) -> Result<CopyOf, ApiError> {
    for s in snap["shards"].as_array().into_iter().flatten() {
        for r in s["replicas"].as_array().into_iter().flatten() {
            if r["url"] == node || r["short"] == node {
                return Ok(CopyOf {
                    shard: s["shard"].as_u64().unwrap_or(0),
                    url: r["url"].as_str().unwrap_or_default().to_string(),
                    short: r["short"].as_str().unwrap_or(node).to_string(),
                    primary: s["primary"].as_str().map(String::from),
                });
            }
        }
    }
    Err(ApiError { code: None, status: 404, message: format!("no copy {node} in this cluster: GET /v1/cluster lists each copy's url and short name") })
}

/// Asks the copy `url` of the cluster to do `kind` (a kind of `Profile`
/// that is not a profile), with `body`.
async fn ask_copy(c: &crate::coord::Coordinator, url: &str, kind: crate::profiling::Kind, body: Json) -> Result<Json, ApiError> {
    let a = crate::profiling::Asked { kind, body, folded: false, in_use: false };
    match c.profile(url, a).await? {
        crate::profiling::Answer::Json(j) => Ok(j),
        crate::profiling::Answer::Folded(..) => Err(ApiError::internal("the copy answered folded lines")),
    }
}

/// `POST /v1/admin/copies/verify {"url", "timeout_ms"?}` on a coordinator:
/// the primary of that copy's shard compares its replicas' rows with its
/// own (as its `POST /v1/admin/verify`), and answers
/// `{replicas: [{url, in_sync, compared, differing_slots,
/// first_differing_slot, seq_no, error}]}`.
async fn copy_verify(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let c = app.svc.coordinator()?;
    let copy = copy_of(&*c.snapshot().await, body_str(&b, "url")?)?;
    let primary = copy.primary.clone().ok_or_else(|| ApiError { code: None, status: 409, message: format!("shard {} has no primary up to compare its copies", copy.shard) })?;
    let by = keys::audit_name(&who(w));
    keys::note_change(keys::Change { object: Some(format!("copy {}", copy.short)), after: Some(json!({"url": copy.url, "primary": primary})), ..Default::default() });
    let mut body = json!({"by": by});
    if let Some(t) = b.get("timeout_ms").and_then(Json::as_u64) {
        body["timeout_ms"] = json!(t);
    }
    let j = ask_copy(c, &primary, crate::profiling::Kind::Verify, body).await?;
    Ok(audited(j, format!("verified shard {}'s copies against its primary, for {}", copy.shard, copy.short)))
}

/// `POST /v1/admin/copies/checkpoint {"url", "name", "layout"?,
/// "link_from"?}` on a coordinator: that copy writes a checkpoint of its
/// database into its own `--backup-dir` (as its
/// `POST /v1/admin/checkpoint`), and answers `{checkpoint}`.
async fn copy_checkpoint(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let c = app.svc.coordinator()?;
    let copy = copy_of(&*c.snapshot().await, body_str(&b, "url")?)?;
    let name = body_str(&b, "name")?.to_string();
    let layout = b["layout"].as_str().unwrap_or("file").to_string();
    let by = keys::audit_name(&who(w));
    keys::note_change(keys::Change { object: Some(format!("copy {}", copy.short)), after: Some(json!({"url": copy.url, "checkpoint": name, "layout": layout})), ..Default::default() });
    let body = json!({"name": name, "layout": layout, "link_from": b["link_from"], "by": by});
    let j = ask_copy(c, &copy.url, crate::profiling::Kind::Checkpoint, body).await?;
    Ok(audited(j, format!("checkpoint {name} of {}", copy.short)))
}

/// `POST /v1/admin/copies/scrub {"url"}` on a coordinator: that copy
/// reads every segment of its database against its checksum (as its
/// `POST /v1/admin/scrub`), and answers `{scrub}`.
async fn copy_scrub(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let c = app.svc.coordinator()?;
    let copy = copy_of(&*c.snapshot().await, body_str(&b, "url")?)?;
    let by = keys::audit_name(&who(w));
    keys::note_change(keys::Change { object: Some(format!("copy {}", copy.short)), after: Some(json!({"url": copy.url, "scrub": true})), ..Default::default() });
    let j = ask_copy(c, &copy.url, crate::profiling::Kind::Scrub, json!({"by": by})).await?;
    let found = j["scrub"]["damaged"].as_array().map_or(0, Vec::len);
    Ok(audited(j, format!("checked {}'s data against its checksums: {}", copy.short, match found {
        0 => "no damage".to_string(),
        1 => "1 damaged segment".to_string(),
        n => format!("{n} damaged segments"),
    })))
}

/// `GET /v1/index/tables` ([`crate::inspect::tables`]).
async fn index_tables(State(app): State<App>) -> Res {
    Ok(AxJson(crate::inspect::tables(&app.svc).await?))
}

/// `GET /v1/health/fleet` ([`crate::fleet`]).
async fn fleet_health(State(app): State<App>) -> AxJson<Json> {
    AxJson(app.svc.fleet_health().await)
}

/// `GET /v1/coordinators` ([`crate::peers`]): fleet. With `window=1`,
/// this server's searches over fleet health's windows too.
async fn coordinators(State(app): State<App>, Query(q): Params) -> AxJson<Json> {
    let mut j = crate::peers::json(&app.svc);
    if q.get("window").is_some_and(|v| v == "1" || v == "true") {
        j["window"] = crate::peers::window_json(&app.svc).await;
    }
    AxJson(j)
}

/// An event row of the log store's `events`, as `GET /v1/events` has it.
fn event_of_row(r: &Json) -> Json {
    let parsed = |k: &str| r[k].as_str().and_then(|t| serde_json::from_str::<Json>(t).ok());
    let mut e = json!({"id": r["id"], "ts_ms": r["ts_ms"], "kind": r["kind"], "severity": r["severity"], "title": r["title"], "node": r["node"]});
    for (k, v) in [("detail", parsed("detail")), ("where", parsed("place")), ("who", r.get("who").cloned()), ("until_ms", r.get("until_ms").cloned())] {
        if let Some(v) = v.filter(|v| !v.is_null()) {
            e[k] = v;
        }
    }
    e
}

/// `GET /v1/events?from=&to=&kind=a,b&limit=` ([`crate::events`]): the
/// events from `from` to `to` (ms; the last 24 hours by default), the
/// newest first; from the log store for what memory no longer holds. Who
/// did what is the admin's to read.
async fn events_list(State(app): State<App>, w: Option<axum::Extension<Principal>>, Query(q): Params) -> Res {
    let limit = param_u64(&q, "limit", 500)?.clamp(1, 5000) as usize;
    let kinds: Vec<String> = q.get("kind").map(|k| k.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    if kinds.iter().any(|k| !k.chars().all(|c| c.is_ascii_lowercase() || c == '_')) {
        return Err(ApiError::bad("kind is a list of event kinds, such as kind=copy_down,primary_changed"));
    }
    // after=<id>: only the events after that one; wait_s: until one
    // comes, for that long at most (30 s), so an agent waits on a job's
    // next event instead of polling.
    let after: Option<i64> = match q.get("after").filter(|s| !s.is_empty()) {
        Some(a) => Some(a.parse().map_err(|_| ApiError::bad("after is an event's id"))?),
        None => None,
    };
    let wait_s = param_u64(&q, "wait_s", 0)?.min(30);
    if let (Some(after), true) = (after, wait_s > 0 && !q.contains_key("to")) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(wait_s);
        loop {
            let now = telemetry::now_ms();
            let (kept, _) = app.svc.events.range(now.saturating_sub(86_400_000), now, &kinds, limit);
            if kept.iter().any(|r| r.id > after) || std::time::Instant::now() >= until {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
    let now = telemetry::now_ms();
    let to = param_u64(&q, "to", now)?;
    let from = param_u64(&q, "from", to.saturating_sub(86_400_000))?;
    let (kept, oldest) = app.svc.events.range(from, to, &kinds, limit);
    let mut events: Vec<Json> = kept.iter().map(|r| json!(**r)).collect();
    let mut source = vec!["memory"];
    let mut error = None;
    // What memory no longer holds (or never held, before a restart).
    let held = app.svc.events.held_from();
    if let (Some(store), true) = (&app.svc.log_store, from < held) {
        let and = (!kinds.is_empty()).then(|| format!("kind IN ({})", kinds.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(", ")));
        match store.kept_rows(crate::logstore::EVENTS, from, held.min(to), and, limit).await {
            Ok(rows) => {
                let ids: std::collections::HashSet<i64> = kept.iter().map(|r| r.id).collect();
                events.extend(rows.iter().filter(|r| r["id"].as_i64().is_none_or(|id| !ids.contains(&id))).map(event_of_row));
                source.push("log_store");
            }
            Err(e) => error = Some(format!("the log store did not answer: {}", e.message)),
        }
    }
    if let Some(after) = after {
        events.retain(|e| e["id"].as_i64().is_some_and(|id| id > after));
    }
    events.sort_by(|a, b| b["ts_ms"].as_u64().cmp(&a["ts_ms"].as_u64()).then(b["id"].as_i64().cmp(&a["id"].as_i64())));
    events.truncate(limit);
    if who(w).may(Scope::Admin, "").is_err() {
        for e in &mut events {
            if let Some(o) = e.as_object_mut() {
                o.remove("who");
            }
        }
    }
    let mut j = json!({"events": events, "from_ms": from, "to_ms": to, "source": source, "memory_from_ms": held, "oldest_in_memory_ms": oldest});
    if let Some(e) = error {
        j["error"] = json!(e);
    }
    Ok(AxJson(j))
}

/// `POST /v1/events/notes {"text", "ts_ms"?, "where"?}`: a note on the
/// timeline, by whoever sent it, audited.
async fn events_note(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    let text = b["text"].as_str().map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| ApiError::bad("a note needs its \"text\""))?;
    if text.chars().count() > 2000 {
        return Err(ApiError::bad("a note is 2000 characters at most"));
    }
    let now = telemetry::now_ms();
    let ts = match b.get("ts_ms") {
        None | Some(Json::Null) => now,
        Some(v) => v.as_u64().filter(|t| *t <= now + 60_000).ok_or_else(|| ApiError::bad("ts_ms is a time in ms since 1970, not one to come"))?,
    };
    let where_ = match b.get("where") {
        None | Some(Json::Null) => Json::Null,
        Some(Json::Object(m)) => {
            let ok = m.iter().all(|(k, v)| match k.as_str() {
                "shard" => v.is_u64(),
                "copy" | "table" | "rule" => v.is_string(),
                _ => false,
            });
            if !ok {
                return Err(ApiError::bad("where is {\"shard\"?: n, \"copy\"?, \"table\"?, \"rule\"?}"));
            }
            Json::Object(m.clone())
        }
        Some(_) => return Err(ApiError::bad("where is an object: {\"shard\"?, \"copy\"?, \"table\"?, \"rule\"?}")),
    };
    let by = keys::audit_name(&who(w));
    let e = crate::events::Event { kind: "note".into(), severity: "info".into(), title: text.to_string(), detail: Json::Null, where_, who: Some(by) };
    let r = app.svc.events.add_at(e, ts, None);
    keys::note_change(keys::Change { kind: Some(keys::NOTE_KIND), object: Some("timeline".into()), after: Some(json!({"text": text, "ts_ms": ts})), ..Default::default() });
    keys::note(format!("noted on the timeline: {}", text.chars().take(100).collect::<String>()));
    Ok(AxJson(json!({"event": *r})))
}

// ----- end core

/// Who the request is from ([`crate::keys`]), and whether that may call
/// its route ([`route_scope`]); what the request then does (a statement, a
/// plan, a logged query, an import that creates tables) is checked by the
/// service, as for gRPC. Requests that need the admin scope, refused ones,
/// and ones the service notes ([`keys::note`]) go to the audit log.
async fn auth(State(app): State<App>, mut req: Request, next: Next) -> Response {
    let t0 = Instant::now();
    // The route's name is shared, not copied.
    let matched = req.extensions().get::<MatchedPath>().cloned();
    let route = matched.as_ref().map_or("other", |p| p.as_str());
    let method = req.method().clone();
    // `X-Strata-Authorization` when there is no `Authorization`: a call
    // through a Kubernetes API server's proxy (the hub's way into a
    // cluster it made, [`crate::hub`]), which takes `Authorization` for
    // itself and does not pass it on.
    let token = header_str(req.headers(), header::AUTHORIZATION).or_else(|| header_str(req.headers(), header::HeaderName::from_static(crate::hub::VIA_PROXY_AUTH)));
    let who = app.svc.keys.resolve(app.token.as_ref(), app.cluster.as_ref(), token);
    // An app that checks its callers runs a caller's statement narrowed
    // to the caller's tables ([`Principal::narrowed`]): checked as theirs.
    let (who, narrowing) = match (who, header_str(req.headers(), header::HeaderName::from_static(keys::AS_TABLES))) {
        (Some(w), Some(g)) => match w.narrowed(g) {
            Ok(n) => (Some(n), None),
            Err(e) => (Some(w), Some(e)),
        },
        (w, _) => (w, None),
    };
    // A refused credential that is a key known here (expired, revoked):
    // its name and id, so that Access names the client still sending it.
    let known = match &who {
        None => app.svc.keys.identify(token),
        Some(_) => None,
    };
    let need = route_scope(&method, route);
    // An app that holds a key limited to tables to its grants takes such
    // keys, at any scope that reads ([`crate::apps::Apps::holds_grants`]).
    let app_grants = route == "/v1/apps/:name/*rest"
        && who.as_ref().is_some_and(|w| w.limited() && w.scope >= Scope::Read)
        && match req.uri().path().strip_prefix("/v1/apps/").and_then(|r| r.split('/').next()) {
            Some(name) => app.svc.apps.holds_grants(name).await,
            None => false,
        };
    // A follow process's writes ([`crate::service::FOLLOW`]).
    let follow = header_str(req.headers(), header::HeaderName::from_static(crate::service::FOLLOW_HEADER)).map(str::to_string);
    let refused = match &who {
        None => Some(ApiError { code: None, status: 401, message: "missing or wrong token".into() }),
        Some(_) if narrowing.is_some() => narrowing,
        // A connector's push key writes its table and reads nothing, an
        // app's included.
        Some(w) if w.push_only() && !crate::scoped::push_route_ok(&method, route) => Some(ApiError {
            code: Some("scope_refused"),
            status: 403,
            message: format!("{method} {route} is not for {:?}, a connector's push key: it writes its table through the connector's push, and reads nothing", w.name),
        }),
        // A key limited to tables calls the routes that check them, and no
        // other ([`crate::scoped`]).
        Some(_) if app_grants => None,
        Some(w) if w.limited() && !crate::scoped::route_ok(&method, route) && !w.allowed_route(&method, route) => w.may_server(&format!("{method} {route}")).err(),
        // A key for operations: the server's views, and for `ops` the
        // bounded actions ([`crate::ops::role_may`]).
        Some(w) if w.scope < need && !crate::ops::role_may(w.role, &method, route) => w.may_by_role(need, &format!("{method} {route}"), crate::ops::role_may(Some(keys::Role::Ops), &method, route)).err(),
        // A tier 3 action (it loses data or costs money) is never an
        // agent's alone, whatever its key (audit X-D2): it files a change
        // request, which a person applies with their own key.
        Some(w) if agent_tier_refused(w, &method, route) => Some(ApiError::coded(
            403,
            "change_request_required",
            format!("{method} {route} loses data or costs money (tier 3): an agent does not do it alone, even with an admin key. Dry run it (POST /v1/admin/changes/preview) and file a change request (POST /v1/admin/changes) for a person to approve"),
        )),
        // An agent says why it changes the server: the audit log keeps it.
        Some(w) if agent_needs_reason(w, &method, route) => Some(ApiError::coded(
            400,
            "reason_required",
            format!("{method} {route} by an agent says why: send the x-chloedb-reason header (a sentence; the audit log keeps it)"),
        )),
        Some(w) if follow.is_some() && !w.root => Some(ApiError { code: None, status: 403, message: format!("{} names a cluster this one follows: only its follow process sends it, with the cluster's token", crate::service::FOLLOW_HEADER) }),
        Some(_) if follow.as_deref().is_some_and(|f| !crate::service::follow_name_ok(f)) => Some(ApiError::bad(format!("{} is 1 to 64 letters, digits, '.', '_' or '-'", crate::service::FOLLOW_HEADER))),
        Some(_) => None,
    };
    let (res, note, change) = match refused {
        // An MCP client reads a JSON-RPC error: why the key may not use the
        // tools (a key limited to tables, whose tools' tables they would not
        // check), with the HTTP status still saying it.
        Some(e) if route == "/v1/mcp" && e.status == 403 => {
            let message = format!("{}; ChloeDB's MCP tools take a key of every table (use the HTTP API with this key: /v1/query, /v1/sql, /v1/write)", e.message);
            let body = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32001, "message": message}});
            let mut r = (StatusCode::FORBIDDEN, AxJson(body)).into_response();
            r.extensions_mut().insert(ClientError(e.message.into()));
            (r, None, keys::Change::default())
        }
        Some(e) => (e.into_response(), None, keys::Change::default()),
        None => {
            // Who the request's queries are from ([`telemetry::Caller`]).
            let caller = match &who {
                Some(w) => telemetry::Caller::of(w, keys::caller()),
                None => telemetry::Caller::of(&Principal::open(), keys::caller()),
            };
            if let Some(w) = &who {
                req.extensions_mut().insert(w.clone());
            }
            let follow: Option<Arc<str>> = follow.map(Into::into);
            let own = who.as_ref().is_some_and(|w| w.root);
            telemetry::calling(Arc::new(caller), crate::service::FOLLOW.scope(follow, crate::service::OWN.scope(own, keys::noting_change(next.run(req))))).await
        }
    };
    let status = res.status().as_u16();
    // What the service noted of the request ([`keys::note`]), or its
    // handler.
    let detail = note.or_else(|| res.extensions().get::<Audited>().map(|a| a.0.clone()));
    // Changes and refusals: a read answered (GET, HEAD) is not audited,
    // unless something noted it.
    let refusal = status == 401 || status == 403;
    // A POST that only reads (a plan asked, a log search, a replay, a
    // cloud's plan or check) changes nothing: a read too.
    let plan = matches!(
        route,
        "/v1/admin/plan" | "/v1/admin/kube/plan" | "/v1/admin/simulator/predict" | "/v1/logs/query" | "/v1/admin/autoscale/replay" | "/v1/admin/aws/plan" | "/v1/admin/aws/validate" | "/v1/admin/digitalocean/plan" | "/v1/admin/digitalocean/validate"
            // alerts agent: a rule replayed.
            | "/v1/alerts/backtest"
    );
    // A read that failed (404, 405, 500) changed nothing either.
    // Coordinators telling each other their alerts: a sync between
    // servers, which the coordinator it came from audited as its change.
    let sync = route == "/v1/alerts/peer";
    let read = method == axum::http::Method::GET || method == axum::http::Method::HEAD || plan || sync;
    // A request that made its own event of what it did (an alert
    // acknowledged, a log level set, a Contain rule): its audit entry is
    // not a `change` event too, so that the Timeline says it once.
    let evented = res.extensions().get::<Evented>().is_some();
    // cluster2: why a request failed (its error), kept in its entry's
    // `after`, so that the Changes list says what went wrong.
    let failed = (status >= 400).then(|| res.extensions().get::<ClientError>().map(|e| e.0.clone()).or_else(|| res.extensions().get::<Failure>().map(|e| e.0.clone()))).flatten();
    // A change through an app is audited as an admin's change is.
    let app_change = route == "/v1/apps/:name/*rest" && !read;
    // An approved change request being applied: audited whatever its route.
    let applying = keys::caller().and_then(|c| c.change).filter(|id| app.svc.oncall.proposal_of(id).is_some_and(|p| p.state == "applying"));
    if refusal || detail.is_some() || ((need == Scope::Admin || app_change) && !read) || (applying.is_some() && !read) {
        let kind = if refusal { keys::REFUSAL } else { change.kind.unwrap_or(keys::CHANGE) };
        let caller = keys::caller().unwrap_or_default();
        // ops: an agent's request, and what it said of itself.
        let agent = who.as_ref().filter(|w| crate::ops::is_agent(w, Some(&caller))).map(|_| {
            let mut a = crate::ops::agent_of(&caller).unwrap_or_else(|| json!({}));
            if let Some(app) = &caller.app {
                a["via"] = json!(app);
            }
            a
        });
        let detail = match (&known, detail) {
            (Some((_, name, why)), None) => Some(format!("refused: the key {name} is {why}")),
            (_, d) => d,
        };
        app.svc.audit_keep(keys::Entry {
            ts_ms: telemetry::now_ms(),
            who: who.as_ref().map_or_else(|| known.as_ref().map_or("unknown".into(), |k| k.1.clone()), keys::audit_name),
            key: who.as_ref().and_then(|w| w.key.clone()).or_else(|| known.as_ref().map(|k| k.0.clone())),
            scope: who.as_ref().map(|w| w.scope.name()),
            method: method.to_string(),
            route: route.to_string(),
            status,
            ms: t0.elapsed().as_secs_f64() * 1e3,
            request_id: obs::current().map(|id| id.to_string()),
            detail,
            kind: kind.into(),
            addr: caller.addr.clone(),
            client: caller.named(),
            reason: caller.reason.clone(),
            object: change.object,
            before: change.before,
            after: change.after.or_else(|| failed.map(|e| json!({"error": &*e}))),
            actor: change.actor.unwrap_or(if agent.is_some() { "agent" } else { "person" }).into(),
            agent,
            on_behalf_of: who.as_ref().and_then(|w| w.on_behalf_of.as_deref().map(String::from)),
            // Only a change request being applied now: the header alone
            // could be any client's.
            change_request: applying,
        }, !evented);
    }
    // A client's mistake (4xx), in one line at info, ten a second at most.
    if (400..500).contains(&status) {
        static LIMIT: obs::RateLimit = obs::RateLimit::new(10);
        if let (Some(e), Some(held)) = (res.extensions().get::<ClientError>(), LIMIT.admit()) {
            let key = who.as_ref().map(keys::audit_name);
            tracing::info!(route = %route, status, error = %e.0, key = key.as_deref().unwrap_or("unknown"), held_back = held, "client error");
        }
    }
    res
}

/// Whether a request is an agent's change of the server that does not
/// say why ([`crate::ops`]): tier 1 and up, not a write of rows.
fn agent_needs_reason(w: &Principal, method: &axum::http::Method, route: &str) -> bool {
    let caller = keys::caller();
    crate::ops::tier(method, route) >= 1
        && !crate::ops::is_data(route)
        // A change request carries its reason in its body; feedback is
        // the command center's own data, triaged as any client does.
        && !route.starts_with("/v1/admin/changes")
        && !route.starts_with("/v1/feedback")
        && crate::ops::is_agent(w, caller.as_ref())
        && caller.as_ref().is_none_or(|c| c.reason.as_deref().is_none_or(|r| r.trim().is_empty()))
}

/// Whether a request is an agent's tier 3 action ([`crate::ops::tier`]),
/// which it files as a change request instead. SQL is judged by its
/// statement, in [`sql`] ([`crate::changes::sql_tier`]). An approved
/// change is applied with the approver's own key and headers that name no
/// agent ([`crate::changes::applying`]), so it is not refused here.
fn agent_tier_refused(w: &Principal, method: &axum::http::Method, route: &str) -> bool {
    crate::ops::tier(method, route) >= 3 && crate::ops::is_agent(w, keys::caller().as_ref())
}

/// Why a request failed on the client's side (4xx), for [`auth`] to log.
#[derive(Clone)]
struct ClientError(Arc<str>);

/// What the audit log says a request did, set on its answer.
#[derive(Clone)]
struct Audited(String);

/// The answer `j`, with what the audit log says of it.
/// The statement budget a request's body asks for (`"budget": {"nodes":
/// 1000, ...}` or `"budget": "nodes=1000"`, [`chloedb::query::budget`]),
/// within its key's and the server's; none when it asks for none.
fn body_budget(b: &Json) -> Result<chloedb::query::budget::Budget, ApiError> {
    Ok(b.get("budget").map(chloedb::query::budget::Budget::from_json).transpose()?.unwrap_or_default())
}

fn audited(j: Json, detail: String) -> Response {
    let mut r = AxJson(j).into_response();
    r.extensions_mut().insert(Audited(detail));
    r
}

/// cluster2: a change that failed, said by what it tried (`what`) in
/// the audit log and on the Timeline rather than by its route (AC-3).
fn tried(what: String) -> impl FnOnce(ApiError) -> Response {
    move |e| {
        let mut r = e.into_response();
        r.extensions_mut().insert(Audited(what));
        r
    }
}

/// On an answer: its request made its own event of what it did, which
/// its audit entry is not ([`auth`]).
#[derive(Clone)]
struct Evented;

/// The answer `r`, whose request made its own event.
fn evented(mut r: Response) -> Response {
    r.extensions_mut().insert(Evented);
    r
}

/// The scope a route needs ([`crate::keys`]). What shows other clients'
/// SQL (the running and latest queries, logs) is the admin's.
pub(crate) fn route_scope(method: &axum::http::Method, route: &str) -> Scope {
    use axum::http::Method;
    match route {
        // All a feed key may read: the feed, and whether it is up.
        "/v1/feed" | "/v1/feed/keys" | "/v1/feed/promoted" | "/v1/health" | "/v1/whoami" => Scope::Feed,
        "/v1/write" | "/v1/import" | "/v1/stream" | "/v1/agent" | "/v1/judge" => Scope::Write,
        // connectors: a system's push, written with its own key.
        "/v1/connectors/:name/push" => Scope::Write,
        "/v1/kv/:table/:key" if method == Method::PUT || method == Method::DELETE => Scope::Write,
        // otlp: logs in; reading the server's own lines stays the admin's.
        "/v1/logs" if method == Method::POST => Scope::Write,
        // The rows of one table need read on it; the files, the admin ([`export`]).
        "/v1/export" => Scope::Read,
        // The autopilot writes its tables, may spend Claude's tokens on
        // grades, and applying a proposal replaces a rank profile.
        "/v1/relevance/autopilot" | "/v1/relevance/proposals/:id/apply" | "/v1/relevance/proposals/:id/dismiss" => Scope::Admin,
        "/v1/synonyms" if method == Method::PUT => Scope::Admin,
        // Mining reads a table's words and compares them pairwise, and
        // proposes a change to ranking: an admin's, as the list it feeds.
        "/v1/synonyms/mine" => Scope::Admin,
        "/v1/alerts/rules" => Scope::Admin,
        "/v1/alerts/routes" | "/v1/alerts/routes/:name/test" => Scope::Admin,
        // alerts2: coordinators exchange their alerts as admins.
        "/v1/alerts/peer" => Scope::Admin,
        // alerts (alerts agent): acknowledging, silencing and noting write.
        "/v1/alerts/:rule/ack" | "/v1/alerts/:rule/silence" | "/v1/alerts/:rule/notes" => Scope::Write,
        "/v1/events/notes" => Scope::Admin,
        // feedback: sending and triaging write; deleting is an admin's.
        "/v1/feedback" if method == Method::POST => Scope::Write,
        "/v1/feedback/:id" if method == Method::PATCH => Scope::Write,
        "/v1/feedback/:id" if method == Method::DELETE => Scope::Admin,
        // An app's reads are a reader's; what changes it (starts a crawl) an
        // admin's, or a writer's for an app that checks its callers itself
        // ([`crate::apps::checks_callers`], checked as it is forwarded).
        "/v1/apps/:name/*rest" if method != Method::GET && method != Method::HEAD => Scope::Write,
        // The inspector names the keys most read.
        // The hub's calls to other clusters carry their keys: its owner's.
        r if r.starts_with("/c/") => Scope::Admin,
        r if r.starts_with("/v1/admin/") || r.starts_with("/v1/queries") || r.starts_with("/v1/logs") || r.starts_with("/v1/profile") || r.starts_with("/v1/index") => Scope::Admin,
        _ => Scope::Read,
    }
}

/// Who a request is from (the open server's root when no auth ran).
fn who(req_who: Option<axum::Extension<Principal>>) -> Principal {
    req_who.map_or_else(Principal::open, |axum::Extension(p)| p)
}

/// Refuses statement `sql` for a key limited to tables when it names a
/// table the key may not read ([`crate::scoped::check_reads`]).
async fn reads_ok(app: &App, who: &Principal, sql: &str) -> Result<(), ApiError> {
    // Before a limited key's tables are read from the statement, which
    // parses it.
    app.svc.statement_fits(sql)?;
    if !who.limited() {
        return Ok(());
    }
    let check = || crate::scoped::check_reads(who, sql, &app.svc.table_names(), &app.svc.functions());
    // N-D5: a coordinator judges by the tables and functions it last
    // learned, and another coordinator may have made a table or changed a
    // stored function since: it learns them again (at most once a second)
    // when the text names a table it does not know or calls a stored
    // function, and judges again if they changed.
    let first = check();
    let stale = matches!(&first, Err(e) if e.message.starts_with("no such table")) || (first.is_ok() && crate::scoped::calls_stored(sql, &app.svc.functions()));
    if stale && app.svc.learn_tables(false).await {
        return check();
    }
    first
}

/// Refuses what a key limited to tables may not do to the table named
/// `table` (the default table when none is), at `need`.
async fn table_ok(app: &App, who: &Principal, table: Option<&str>, need: Scope, what: &str) -> Result<(), ApiError> {
    if !who.limited() {
        return Ok(());
    }
    let name = match table {
        Some(t) => app.svc.table(t).await.map(|s| s.table.clone()).unwrap_or_else(|_| t.to_string()),
        None => app.svc.default_table()?.table.clone(),
    };
    who.may_table(need, &name, what)
}

/// Keeps out pages on other sites: one that reached this server through
/// DNS rebinding (a `Host` it does not answer to), and one that posts to
/// it (an `Origin` other than its own), which a browser lets any page do
/// without a token.
/// The answer that refuses the request, if it is refused ([`observe`]).
fn browsers(app: &App, req: &Request) -> Option<Response> {
    let headers = req.headers();
    let host = header_str(headers, header::HOST).or_else(|| req.uri().authority().map(|a| a.as_str()));
    let why = auth::browser_check(app.hosts.as_deref(), host, header_str(headers, header::ORIGIN), req.method().is_safe())?;
    Some(ApiError { code: None, status: 403, message: why }.into_response())
}

/// Builds a table's slice into the server's `--slices` directory
/// ([`chloedb::slice::build`]). `{"table"?, "text", "lat", "lon", "lat0",
/// "lon0", "tile_m"?, "k1"?, "b"?}` is a geo slice, the grid's origin at
/// (`lat0`, `lon0`) and its tiles `tile_m` metres on a side (1,000 by
/// default); `{"table"?, "vector", "clusters"?, "seed"?, "codes"?}` a
/// vector slice of that many k-means clusters (about the square root of
/// the rows by default), with every row's codes unless `codes` is false. `{"table"}` alone rebuilds a table's slice with the options
/// it was built with.
async fn admin_slices(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let table = b.get("table").and_then(|v| v.as_str()).map(str::to_string);
    let opts = if let Some(field) = b.get("vector") {
        let field = field.as_str().ok_or_else(|| ApiError::bad("\"vector\" is the name of a VECTOR field"))?;
        let n = |k: &str, d: u64| b.get(k).map_or(Ok(d), |v| v.as_u64().ok_or_else(|| ApiError::bad(format!("{k:?} is a whole number"))));
        let clusters = n("clusters", 0)?;
        if clusters > 65_536 {
            return Err(ApiError::bad("a vector slice has at most 65,536 clusters"));
        }
        let codes = b.get("codes").map_or(Ok(true), |v| v.as_bool().ok_or_else(|| ApiError::bad("\"codes\" is true or false")))?;
        chloedb::slice::BuildOptions { table, key: chloedb::slice::BuildKey::Vector { field: field.to_string(), clusters: clusters as usize, seed: n("seed", crate::slices::SEED)?, codes } }
    } else if b.get("text").is_none() && b.get("lat0").is_none() {
        let table = table.ok_or_else(|| ApiError::bad("the body needs a table to rebuild, text, lat, lon, lat0 and lon0 to build a geo slice, or vector to build a vector slice"))?;
        app.svc.slice_options(&table)?
    } else {
        let f = |k: &str| b.get(k).and_then(|v| v.as_f64()).ok_or_else(|| ApiError::bad(format!("the body needs a number {k:?}")));
        let mut bm25 = chloedb::text::Bm25Params::default();
        if let Some(k1) = b.get("k1").and_then(|v| v.as_f64()) {
            bm25.k1 = k1 as f32;
        }
        if let Some(x) = b.get("b").and_then(|v| v.as_f64()) {
            bm25.b = x as f32;
        }
        let grid = chloedb::slice::Grid::new(f("lat0")?, f("lon0")?, b.get("tile_m").and_then(|v| v.as_f64()).unwrap_or(1000.0))?;
        let key = chloedb::slice::BuildKey::Geo { text: body_str(&b, "text")?.to_string(), lat: body_str(&b, "lat")?.to_string(), lon: body_str(&b, "lon")?.to_string(), grid, bm25 };
        chloedb::slice::BuildOptions { table, key }
    };
    let t0 = Instant::now();
    let built = app.svc.build_slice(opts).await?;
    let m = &built.manifest;
    let mut out = json!({
        "table": m.schema.table, "kind": m.kind(), "version": m.version, "rows": built.rows, "tiles": m.tiles.len(),
        "manifest_bytes": built.manifest_bytes, "tile_bytes": built.tile_bytes, "took_ms": t0.elapsed().as_secs_f64() * 1e3,
    });
    match &m.key {
        chloedb::slice::Key::Geo(g) => out["words"] = json!(g.words.terms.len()),
        chloedb::slice::Key::Vector(v) => {
            out["clusters"] = json!(v.centroids.len());
            out["codes_bytes"] = json!(v.codes.as_ref().map(|c| c.bytes));
        }
    }
    Ok(AxJson(out))
}

/// Every slice the server has, with whether its table changed since it
/// was built and how devices use it.
async fn admin_slices_list(State(app): State<App>) -> Res {
    let Ok(slices) = app.svc.slices() else {
        return Ok(AxJson(json!({"enabled": false, "slices": []})));
    };
    let db = app.svc.admin_db()?;
    let list: Vec<Json> = slices.tables().iter().filter_map(|t| slices.describe(t, &db, false).ok()).collect();
    let tables: Vec<Json> = db.catalog().iter().map(|s| json!({"table": s.table, "fields": s.fields.iter().map(|f| json!({"name": f.name, "type": f.type_name()})).collect::<Vec<_>>()})).collect();
    Ok(AxJson(json!({"enabled": true, "dir": slices.dir(), "slices": list, "tables": tables})))
}

/// One slice: what it holds, each tile's rows, bytes and downloads, and
/// how devices used it in the last hour, by minute.
async fn admin_slice(State(app): State<App>, axum::extract::Path(table): axum::extract::Path<String>) -> Res {
    let db = app.svc.admin_db()?;
    Ok(AxJson(app.svc.slices()?.describe(&table, &db, true)?))
}

/// Deletes a table's slice: devices then find none and ask the cloud.
async fn admin_slice_delete(State(app): State<App>, axum::extract::Path(table): axum::extract::Path<String>) -> Res {
    Ok(AxJson(json!({"deleted": app.svc.slices()?.remove(&table)?})))
}

/// Whether a device's slice is the current one.
async fn slice_status(State(app): State<App>, axum::extract::Path(table): axum::extract::Path<String>) -> Res {
    let db = app.svc.admin_db()?;
    Ok(AxJson(app.svc.slices()?.status(&table, &db)?))
}

/// A device's searches since its last report ([`crate::slices::Slices::report`]).
async fn slice_usage(State(app): State<App>, axum::extract::Path(table): axum::extract::Path<String>, AxJson(b): AxJson<Json>) -> Res {
    app.svc.slices()?.report(&table, &b)?;
    Ok(AxJson(json!({})))
}

/// A table's slice manifest, as `POST /v1/admin/slices` last built it.
async fn slice_manifest(State(app): State<App>, axum::extract::Path(table): axum::extract::Path<String>) -> Result<Response, ApiError> {
    let (r, n) = slice_file(&app, &table, "manifest.json", "application/json").await?;
    app.svc.slices()?.note_manifest(&table, n);
    Ok(r)
}

/// One tile of a table's slice (`<x>_<y>.jsonl.gz`).
async fn slice_tile(State(app): State<App>, axum::extract::Path((table, file)): axum::extract::Path<(String, String)>) -> Result<Response, ApiError> {
    let xy = file.strip_suffix(".jsonl.gz").and_then(|t| t.split_once('_')).and_then(|(x, y)| Some((x.parse::<i32>().ok()?, y.parse::<i32>().ok()?)));
    let Some((x, y)) = xy else {
        return Err(ApiError::bad(format!("{file:?} is not a tile's file (<x>_<y>.jsonl.gz)")));
    };
    let (r, n) = slice_file(&app, &table, &format!("tiles/{file}"), "application/gzip").await?;
    app.svc.slices()?.note_tile(&table, x, y, n);
    Ok(r)
}

/// A vector slice's codes of every row (`codes.bin.gz`).
async fn slice_codes(State(app): State<App>, axum::extract::Path(table): axum::extract::Path<String>) -> Result<Response, ApiError> {
    let (r, n) = slice_file(&app, &table, chloedb::slice::CODES_FILE, "application/gzip").await?;
    app.svc.slices()?.note_codes(&table, n);
    Ok(r)
}

async fn slice_file(app: &App, table: &str, rel: &str, ty: &'static str) -> Result<(Response, u64), ApiError> {
    let path = app.svc.slice_dir(table)?.join(rel);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let n = bytes.len() as u64;
            Ok((([(header::CONTENT_TYPE, ty), (header::CACHE_CONTROL, "no-cache")], bytes).into_response(), n))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ApiError { code: None, status: 404, message: format!("no {rel} in table {table}'s slice (build it with POST /v1/admin/slices)") }),
        Err(e) => Err(ApiError::internal(e)),
    }
}

fn body_str<'a>(b: &'a Json, k: &str) -> Result<&'a str, ApiError> {
    b.get(k).and_then(|v| v.as_str()).ok_or_else(|| ApiError::bad(format!("the body needs a string {k:?}")))
}

/// The whole number `k` of body `b`, 0 when it is missing or null; refused
/// when it is anything else (`"100"`, `-1`, `1e15`), rather than read as
/// missing.
fn body_u64(b: &Json, k: &str) -> Result<u64, ApiError> {
    match b.get(k) {
        None | Some(Json::Null) => Ok(0),
        Some(v) => v.as_u64().ok_or_else(|| ApiError::bad(format!("{k} is a whole number from 0{}, not {}", if k.ends_with("_ms") { " (milliseconds)" } else { "" }, crate::obs::truncate(&v.to_string(), 40)))),
    }
}

/// The fields of a query's body (`POST /v1/query`).
const QUERY_FIELDS: &[&str] = &["sql", "params", "timeout_ms", "shard", "stats", "snapshot", "partial", "min_seq_no", "min_seq_nos", "profile", "trace", "steps", "page_size", "keep_alive_ms", "cursor", "format", "log", "objects", "reason"];

/// A sentence for each field of body `b` that is not one of `known` (a
/// misspelt `timeout_ms` would otherwise go unnoticed), for an answer's
/// `warnings`.
fn unknown_fields(b: &Json, known: &[&str]) -> Vec<String> {
    b.as_object().map_or_else(Vec::new, |o| o.keys().filter(|k| !known.contains(&k.as_str())).map(|k| format!("{k:?} is not a field of this request: it was left out (the fields are {})", known.join(", "))).collect())
}

/// `j` with `warnings` for the fields of `b` that are not `known`.
fn warn_unknown(j: &mut Json, b: &Json, known: &[&str]) {
    let w = unknown_fields(b, known);
    if !w.is_empty() {
        j["warnings"] = json!(w);
    }
}

/// The writes a read must see ([`After`]): the body's `min_seq_no`, the
/// sequence number a server's write answered, or `min_seq_nos`, the
/// numbers a coordinator's answered, one per shard.
fn after(b: &Json) -> Result<After, ApiError> {
    let seq = |v: &Json, what: &str| v.as_u64().ok_or_else(|| ApiError::bad(format!("{what} is a sequence number: a whole number from 0")));
    let min = match b.get("min_seq_no") {
        None | Some(Json::Null) => 0,
        Some(v) => seq(v, "min_seq_no")?,
    };
    let mins = match b.get("min_seq_nos") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(a)) => a.iter().map(|v| seq(v, "each of min_seq_nos")).collect::<Result<_, _>>()?,
        Some(_) => return Err(ApiError::bad("min_seq_nos is a list of sequence numbers, one per shard, as a coordinator's write answers seq_nos")),
    };
    After::of(min, mins)
}

async fn health(State(app): State<App>) -> Response {
    let h = app.svc.health().await;
    let code = if matches!(h["status"].as_str(), Some("down" | "failing")) { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    (code, AxJson(h)).into_response()
}

/// Liveness and readiness: 503 when the server cannot write until it is
/// restarted ([`Service::failing`]). Neither asks a coordinator's shards,
/// so a slow shard cannot take every coordinator out of service at once;
/// `/v1/health` shows them.
async fn live(State(app): State<App>) -> Response {
    match app.svc.failing() {
        Some(why) => (StatusCode::SERVICE_UNAVAILABLE, AxJson(json!({"status": "failing", "error": why}))).into_response(),
        None => AxJson(json!({"status": "ok"})).into_response(),
    }
}

/// Live, and not stopping: a server serving out its drain time
/// ([`crate::server::draining`]) answers 503, so that a load balancer
/// sends it no more clients.
async fn ready(state: State<App>) -> Response {
    if crate::server::draining() {
        return (StatusCode::SERVICE_UNAVAILABLE, AxJson(json!({"status": "draining", "error": "the server is stopping: it ends the requests under way and takes no new ones"}))).into_response();
    }
    live(state).await
}

/// While the server drains, each answer closes its connection: the
/// client's next request opens another, which a load balancer sends to a
/// server that stays.
fn close_when_draining(r: &mut Response) {
    if crate::server::draining() {
        r.headers_mut().insert(axum::http::header::CONNECTION, axum::http::HeaderValue::from_static("close"));
    }
}

async fn stats(State(app): State<App>) -> AxJson<Json> {
    AxJson(app.svc.stats().await)
}

/// A table's schema: `?table=`, else the first table's.
async fn schema(State(app): State<App>, w: Option<axum::Extension<Principal>>, Query(q): Query<std::collections::HashMap<String, String>>) -> Res {
    let who = who(w);
    let s = match q.get("table") {
        Some(t) => {
            // A table it may not read is refused whether or not it exists.
            if who.limited() {
                who.may_table(Scope::Read, t, "a table's schema")?;
            }
            // A table there is none of is not found, as a row is.
            app.svc.table(t).await.map_err(|e| if e.status == 400 && crate::errors::code_of(&e) == "table_not_found" { ApiError { status: 404, ..e } } else { e })?
        }
        // A key limited to tables: the first table it may read, rather than
        // a refusal of one it did not name.
        None if who.limited() => app.svc.tables_listed().await.iter().find(|s| who.table_scope(&s.table).is_some_and(|x| x >= Scope::Read)).cloned().ok_or_else(|| ApiError::bad(format!("the key {:?} may read no table yet: it is limited to {}", who.name, who.tables.as_deref().map(keys::grants_text).unwrap_or_default())))?,
        None => app.svc.default_table()?,
    };
    who.may_table(Scope::Read, &s.table, "a table's schema")?;
    Ok(AxJson(table_json(&app.svc, &s)))
}

/// Every table's schema, the first first: those the caller may read, for
/// a key limited to tables.
async fn tables(State(app): State<App>, w: Option<axum::Extension<Principal>>) -> AxJson<Json> {
    let who = who(w);
    let tables: Vec<Json> = app.svc.tables_listed().await.iter().filter(|s| who.table_scope(&s.table).is_some_and(|x| x >= Scope::Read)).map(|s| table_json(&app.svc, s)).collect();
    AxJson(json!({"tables": tables}))
}

/// `GET /v1/functions`: the stored functions queries call
/// ([`chloedb::query::function`]), by name: each one's current version
/// (none once dropped) and every version kept, oldest first.
async fn functions(State(app): State<App>) -> Res {
    app.svc.learn_functions().await;
    let set = app.svc.functions();
    let mut out: Vec<Json> = Vec::new();
    let mut names: Vec<&str> = set.all().map(|f| f.def.name.as_str()).collect();
    names.dedup();
    for name in names {
        let versions: Vec<Json> = set
            .all()
            .filter(|f| f.def.name == name)
            .map(|f| {
                json!({
                    "version": f.version, "dropped": f.dropped, "kind": f.def.kind.name(), "signature": f.def.signature(), "returns": f.def.returns,
                    "body": (!f.dropped).then_some(&f.def.body), "description": f.def.description, "hash": f.hash,
                    "created": f.created.map(chloedb::timestamp::format), "created_by": f.created_by,
                    "sql": (!f.dropped).then(|| f.def.sql(false, None)),
                })
            })
            .collect();
        out.push(json!({"name": name, "current": set.current(name).map(|f| f.version), "versions": versions}));
    }
    Ok(AxJson(json!({"functions": out})))
}

/// `GET /v1/index?table=&sample=&seed=&terms=&top=`: the index
/// inspector's statistics of a table ([`crate::inspect`]): this server's,
/// or a coordinator's per replica, per shard and for its cluster.
async fn index(State(app): State<App>, Query(q): Params) -> Res {
    let a = crate::inspect::Asked::of(&json!(q))?;
    Ok(AxJson(crate::inspect::answer(&app.svc, a).await?))
}

/// `POST /v1/index/reads/reset`: this server's read counts start again
/// ([`crate::reads`]), and every other coordinator's (fleet).
async fn reset_reads(State(app): State<App>, headers: HeaderMap) -> AxJson<Json> {
    app.svc.reads.reset();
    let mut j = json!({"reset": true, "at_ms": crate::telemetry::now_ms()});
    if crate::peers::from_peer(&headers).is_none() {
        let answers = crate::peers::post_all(&app.svc, "/v1/index/reads/reset", &json!({})).await;
        j["coordinators"] = crate::peers::reach(&app.svc, &answers);
    }
    AxJson(j)
}

/// `GET /v1/index/reads?table=`: this server's reads of a table, by the
/// copy that served them ([`crate::inspect::reads_json`]).
async fn index_reads(State(app): State<App>, Query(q): Params) -> Res {
    let table = q.get("table").filter(|t| !t.is_empty()).ok_or_else(|| ApiError::bad("name the table: ?table=products"))?;
    Ok(AxJson(crate::inspect::reads_json(&app.svc, table)))
}

/// `GET /v1/tables/:table/paths?field=&path=&where=`: a JSON field's
/// paths listing ([`chloedb::query::paths`]).
async fn json_paths(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(table): axum::extract::Path<String>, Query(q): Params) -> Res {
    table_ok(&app, &who(w), Some(&table), Scope::Read, "a table's JSON paths").await?;
    let t0 = Instant::now();
    let field = q.get("field").filter(|f| !f.is_empty()).ok_or_else(|| ApiError::bad("name the JSON field: ?field=attrs"))?;
    let prefix = q.get("path").or_else(|| q.get("prefix")).filter(|p| !p.is_empty() && *p != "$");
    let call = chloedb::query::paths::Call {
        table,
        field: field.clone(),
        prefix: prefix.map(|p| chloedb::json::parse_path(p)).transpose()?,
        filter: q.get("where").filter(|w| !w.trim().is_empty()).cloned(),
    };
    let paths = app.svc.json_paths(&call, &chloedb::query::Params::new(), &crate::service::After::Any, param_u64(&q, "timeout_ms", 0)?).await?;
    let mut j = paths.to_json();
    j["table"] = json!(call.table);
    j["field"] = json!(call.field);
    j["took_ms"] = json!(t0.elapsed().as_secs_f64() * 1e3);
    Ok(AxJson(j))
}

fn table_json(svc: &crate::service::Service, s: &chloedb::schema::Schema) -> Json {
    json!({
        "table": s.table,
        "schema": s,
        "short": s.to_short_json(),
        // The CREATE TABLE statement that makes it.
        "sql": chloedb::query::ddl::create_sql(s),
        "primary_key": s.primary_key.map(|k| s.fields[k].name.clone()),
        "shard_by": s.shard_by,
        // Its rank profiles, by name.
        "profiles": svc.rank_profiles().into_iter().filter(|(t, _)| t.eq_ignore_ascii_case(&s.table)).map(|(_, p)| p).collect::<Vec<_>>(),
        // Each embedded VECTOR field: its sources, model, and whether this
        // server can embed text for it.
        "embeddings": svc.models().iter().filter(|m| m.table.eq_ignore_ascii_case(&s.table)).map(|m| json!({
            "field": m.field, "from": m.spec.from, "model": m.spec.model,
            "loaded": m.model.is_ok(), "error": m.model.as_ref().err(),
            // A model behind an API: the requests it sent.
            "usage": m.model.as_ref().ok().and_then(|e| e.usage()).map(|u| json!({"requests": u.requests, "retries": u.retries, "failures": u.failures, "texts": u.texts, "tokens": u.tokens})),
            // Rows stored without a vector, filled in once the model opened.
            "backfill": svc.backfill_of(&m.table, &m.field).map(|b| json!({"state": b.state, "missing": b.missing, "embedded": b.embedded, "error": b.error})),
        })).collect::<Vec<_>>(),
        // Each derived TEXT field: its MEDIA field, model, and whether this
        // server makes its text (null on a coordinator: its shards do).
        "derived": s.fields.iter().filter_map(|f| f.derive.as_ref().map(|d| json!({
            "field": f.name, "from": d.from, "model": d.model,
            "loaded": svc.local_db().map(|db| db.deriver_in(&s.table, &f.name).is_some()),
        }))).collect::<Vec<_>>(),
    })
}

async fn config(State(app): State<App>) -> AxJson<Json> {
    let a = &app.svc.agent;
    let rules = app.svc.synonyms().map_or(0, |s| s.rules());
    AxJson(json!({"agent": {"model": a.model, "server_key": a.api_key.is_some(), "models": agent::MODELS}, "synonyms": {"rules": rules, "editable": app.svc.local_db().is_some()}}))
}

async fn metrics(State(app): State<App>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], app.svc.prometheus().await)
}

async fn query(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let arrow = match b.get("format") {
        None | Some(Json::Null) => false,
        Some(f) if f == "json" => false,
        Some(f) if f == "arrow" => true,
        Some(f) => return Err(ApiError::bad(format!("format is \"json\" (the default) or \"arrow\", not {f}"))),
    };
    let who = who(w);
    // A scroll: `page_size` starts one, its `cursor` reads the next page.
    let given = |k: &str| b.get(k).is_some_and(|v| !v.is_null());
    let keep_alive = match b.get("keep_alive_ms") {
        None | Some(Json::Null) => None,
        Some(v) => match v.as_u64() {
            Some(ms) if ms > 0 && ms <= crate::service::MAX_KEEP_ALIVE.as_millis() as u64 => Some(Duration::from_millis(ms)),
            _ => return Err(ApiError::bad(format!("keep_alive_ms is how long a scroll is kept after each page: 1 to {} ms", crate::service::MAX_KEEP_ALIVE.as_millis()))),
        },
    };
    if given("cursor") {
        let cursor = b["cursor"].as_str().ok_or_else(|| ApiError::bad("cursor is the string a scroll's last page answered"))?;
        if let Some(k) = ["sql", "params", "page_size", "log", "shard", "stats", "snapshot", "min_seq_no", "min_seq_nos", "profile", "partial"].into_iter().find(|k| given(k)) {
            return Err(ApiError::bad(format!("a scroll's next page takes its cursor alone (with format, keep_alive_ms or timeout_ms), not {k}: the query and its version are those it started with")));
        }
        let a = app.svc.scroll_next(cursor, &crate::service::scroll_owner(&who), keep_alive, body_u64(&b, "timeout_ms")?, None).await?;
        if arrow {
            return arrow_answer(a).await;
        }
        let mut j = answer_of(&b, &a)?;
        warn_unknown(&mut j, &b, QUERY_FIELDS);
        return Ok(AxJson(j).into_response());
    }
    if who.limited() {
        // A shard's query is a coordinator's, not for a key limited to
        // tables; a logged one writes the log's table, which
        // [`Service::query_logged`] checks.
        if given("shard") {
            return Err(who.may_server("a query with shard").unwrap_err());
        }
        reads_ok(&app, &who, body_str(&b, "sql")?).await?;
    }
    let page = match b.get("page_size") {
        None | Some(Json::Null) => None,
        Some(v) => match v.as_u64() {
            Some(n) if n > 0 => Some(n as usize),
            _ => return Err(ApiError::bad("page_size is the most rows a scroll's page holds (1 or more; cut to --max-result-window)")),
        },
    };
    if page.is_none() && keep_alive.is_some() {
        return Err(ApiError::bad("keep_alive_ms is a scroll's: start one with page_size, or ask for its next page with its cursor"));
    }
    let stats = match b.get("stats") {
        None | Some(Json::Null) => None,
        Some(s) => Some(serde_json::from_value(s.clone()).map_err(|e| ApiError::bad(format!("bad stats: {e}")))?),
    };
    let req = QueryReq {
        sql: body_str(&b, "sql")?.to_string(),
        params: app.svc.params_for(b.get("params"), Some(body_str(&b, "sql")?), &who).await?,
        timeout_ms: body_u64(&b, "timeout_ms")?,
        shard: b.get("shard").and_then(|v| v.as_bool()).unwrap_or(false),
        stats,
        snapshot: body_u64(&b, "snapshot")?,
        partial: b.get("partial").and_then(|v| v.as_bool()),
        after: after(&b)?,
        profile: b.get("profile").and_then(|v| v.as_bool()) == Some(true),
        scroll: page.map(|page| crate::service::ScrollAsk { page, keep_alive, owner: crate::service::scroll_owner(&who) }),
        who: Some(who.clone()),
        budget: body_budget(&b)?,
        ..Default::default()
    };
    // The answer cache knows the query by its SQL and the statistics it
    // was sent ([`crate::answers`]); its parameters are added after they
    // are read.
    let cache_key = (!crate::answers::volatile(&req.sql)).then(|| {
        let stats = b.get("stats").map(|s| s.to_string()).unwrap_or_default();
        crate::answers::KeyBuilder::default().part(b"http").part(req.sql.as_bytes()).part(stats.as_bytes()).finish()
    });
    let req = QueryReq { cache_key, ..req };
    // A client's answer has its steps when asked for ([`wants_steps`]); a
    // shard's has them for its coordinator.
    let req = QueryReq { skip_steps: !req.shard && !wants_steps(&b), ..req };
    if let Some(log) = b.get("log") {
        if req.shard {
            return Err(ApiError::bad("a shard's query is not logged: log through the coordinator"));
        }
        if arrow {
            return Err(ApiError::bad("a logged query answers JSON: leave out format"));
        }
        if req.scroll.is_some() {
            return Err(ApiError::bad("a logged query answers at once: leave out page_size"));
        }
        return logged(&app, &who, req, log, body_bool(&b, "objects")?).await.map(IntoResponse::into_response);
    }
    if arrow && req.shard {
        return Err(ApiError::bad("a shard's query answers its coordinator over gRPC: leave out format"));
    }
    if req.shard && req.scroll.is_some() {
        return Err(ApiError::bad("a shard answers its coordinator at once: leave out page_size"));
    }
    let mut a = app.svc.query(req).await?;
    hide_hosts(&who, &mut a.rs.stats.steps);
    if arrow {
        return arrow_answer(a).await;
    }
    let mut j = answer_of(&b, &a)?;
    // `"trace": true`: the timed steps the request took; `"profile":
    // true` answers them too, beside its steps' CPU time.
    if b.get("trace").and_then(|v| v.as_bool()) == Some(true) || b.get("profile").and_then(|v| v.as_bool()) == Some(true) {
        j["trace"] = json!(telemetry::trace().map(|t| t.spans()).unwrap_or_default());
    }
    warn_unknown(&mut j, &b, QUERY_FIELDS);
    Ok(AxJson(j).into_response())
}

/// A query's answer as an Arrow IPC stream of one batch, for clients that
/// want columns (`"format": "arrow"`): no JSON to write or read, and each
/// column typed as an Arrow answer in a client's own process is
/// ([`chloedb::query::arrow::answer_batch`]). How long it took and what the
/// engine did go in headers; an answer without some shards (`partial`)
/// names them in `x-chloedb-missing-shards`. Each header goes under its
/// old name too (`x-strata-missing-shards`), for clients from before the
/// rename.
#[cfg(feature = "datafusion")]
async fn arrow_answer(a: crate::service::Answer) -> Result<Response, ApiError> {
    let took = a.took_ms;
    let missing = a.missing.join(",");
    let cursor = a.cursor.clone();
    let (bytes, stats) = tokio::task::spawn_blocking(move || {
        let (batch, stats) = chloedb::query::arrow::answer_batch(chloedb::query::exec::Leaf::Rows(a.rs))?;
        Ok::<_, chloedb::Error>((chloedb::query::arrow::ipc_of(&batch)?, stats))
    })
    .await
    .map_err(ApiError::internal)??;
    let mut r = (StatusCode::OK, [(header::CONTENT_TYPE, "application/vnd.apache.arrow.stream")], bytes).into_response();
    // A header holds visible ASCII: one that would not is left out rather
    // than failing the answer.
    let mut put = |name: &str, v: String| {
        if let Ok(v) = axum::http::HeaderValue::from_str(&v) {
            for n in crate::names::both(name) {
                r.headers_mut().insert(n, v.clone());
            }
        }
    };
    put("took-ms", format!("{took:.3}"));
    put("stats", serde_json::to_string(&stats).unwrap_or_default());
    if !missing.is_empty() {
        put("missing-shards", missing);
    }
    if let Some(c) = cursor {
        put("cursor", c);
    }
    Ok(r)
}

#[cfg(not(feature = "datafusion"))]
async fn arrow_answer(_: crate::service::Answer) -> Result<Response, ApiError> {
    Err(ApiError::bad("this server was built without Arrow answers (feature datafusion): leave out format"))
}

/// A query run and logged ([`Service::query_logged`]): its answer, with
/// the request's id (`"request"`), an interleaved query's teams
/// (`"teams"`: per row, the ranking that placed it) and the log's write
/// (`"logged"`: the rows, and the write's sequence numbers as a write's
/// answer gives them).
async fn logged(app: &App, who: &Principal, req: QueryReq, log: &Json, objects: bool) -> Res {
    let log = chloedb::rank_log::Log::from_json(log)?;
    let skip = req.skip_steps;
    let (mut a, served, w) = app.svc.query_logged(who, req, log).await?;
    hide_hosts(who, &mut a.rs.stats.steps);
    let mut j = answer(&a);
    if skip {
        without_steps(&mut j);
    }
    if objects {
        as_objects(&mut j);
    }
    j["request"] = json!(served.request);
    if !served.teams.is_empty() {
        j["teams"] = json!(served.teams);
    }
    let mut l = json!({"rows": w.rows});
    if let Some(s) = w.seq_no {
        l["seq_no"] = json!(s);
    }
    if !w.seq_nos.is_empty() {
        l["seq_nos"] = json!(w.seq_nos);
    }
    j["logged"] = l;
    Ok(AxJson(j))
}

/// `GET /v1/feed?from=n&after_term=t` (a coordinator: `&shard=i`): the
/// changes of the server's numbered transactions from `from` on (default
/// 1), or of shard `i`'s through its primary, as NDJSON that goes on as
/// they are written ([`crate::service::Service::feed`]). `after_term` is
/// the term of transaction `from - 1` as the reader holds it. With
/// `copy=true`, a copy of the rows first (`since=n`: those changed after
/// transaction n, whose term `after_term` is then; `whole=a,b`: but every
/// row of tables a and b; `digests=true`: with the digests of the keys). A
/// failure after the answer started ends it with `{"error": ..., "status": ..}`.
///
/// The feed ends when the server stops taking connections
/// ([`crate::server::Stopping`]), so that a server a follower reads from
/// stops rather than waiting on it.
async fn feed(State(app): State<App>, w: Option<axum::Extension<Principal>>, stopping: Option<axum::Extension<crate::server::Stopping>>, Query(q): Query<std::collections::HashMap<String, String>>) -> Result<Response, ApiError> {
    let stopping = Box::pin(crate::server::Stopping::of(stopping.map(|axum::Extension(s)| s)));
    // A key limited to tables reads the feed of the tables it may read.
    let mut only = w.map(|axum::Extension(w)| w).filter(Principal::limited).map(FeedFor::new);
    let num = |k: &str| -> Result<Option<u64>, ApiError> { q.get(k).map(|v| v.parse::<u64>().map_err(|_| ApiError::bad(format!("{k} is a whole number, not {v:?}")))).transpose() };
    let (from, after) = (num("from")?.unwrap_or(1), num("after_term")?.filter(|&t| t > 0));
    let shard = num("shard")?;
    let whole: Vec<String> = q.get("whole").into_iter().flat_map(|w| w.split(',')).filter(|t| !t.is_empty()).map(String::from).collect();
    let copy = flag(&q, "copy", false)?.then_some(crate::service::feed::CopyOpts { since: num("since")?.unwrap_or(0), whole, digests: flag(&q, "digests", false)? });
    if copy.is_none() && ["since", "whole", "digests"].iter().any(|k| q.contains_key(*k)) {
        return Err(ApiError::bad("since, whole and digests are a copy's (copy=true)"));
    }
    if copy.is_some() && only.is_some() {
        return Err(ApiError { code: None, status: 403, message: "a key limited to tables follows the feed from a transaction (from=N); a copy (copy=true) is of every table".into() });
    }
    let ndjson = |body: axum::body::Body| ([(header::CONTENT_TYPE, "application/x-ndjson")], body).into_response();
    if let Ok(c) = app.svc.coordinator() {
        let shard = shard.ok_or_else(|| ApiError::bad("a cluster's feed is each shard's: name one (?shard=0 for the first)"))?;
        let chunks = c.feed(shard as usize, from, after.unwrap_or(0), copy).await?;
        let lines = chunks.map(move |c| {
            Ok::<_, std::convert::Infallible>(match c {
                Ok(c) => match &mut only {
                    Some(f) => f.chunk(&c.lines),
                    None => Bytes::from(c.lines),
                },
                Err(s) => {
                    let e = ApiError::from(s);
                    Bytes::from(format!("{}\n", json!({"error": e.message, "status": e.status})))
                }
            })
        });
        return Ok(ndjson(axum::body::Body::from_stream(futures::StreamExt::take_until(lines, stopping))));
    }
    // A shard's own server serves its own feed.
    let own = app.svc.slots().map_or(0, |(_, s)| s as u64);
    if shard.is_some_and(|s| s != own) {
        return Err(ApiError::bad(format!("this server holds shard {own} alone: ask its coordinator for shard {}", shard.unwrap_or(0))));
    }
    let rx = app.svc.feed(from, after, copy).await?;
    let lines = tokio_stream::wrappers::ReceiverStream::new(rx).map(move |v| {
        Ok::<_, std::convert::Infallible>(match &mut only {
            Some(f) => f.chunk(&v),
            None => Bytes::from(v),
        })
    });
    Ok(ndjson(axum::body::Body::from_stream(futures::StreamExt::take_until(lines, stopping))))
}

/// The feed as a key limited to tables reads it: each transaction's
/// operations on the tables it may read (an empty `ops` when there are
/// none, so that the reader still counts the transaction), without the
/// stream sources' offsets (`sources`, other clients') and the operations
/// of tables the server could not read (`unknown`). The other lines (where
/// the feed starts, how far it reaches, slot marks, an end) are as they
/// are. Lines are taken whole, across chunks.
struct FeedFor {
    who: Principal,
    carry: Vec<u8>,
}

impl FeedFor {
    fn new(who: Principal) -> FeedFor {
        FeedFor { who, carry: Vec::new() }
    }

    fn chunk(&mut self, b: &[u8]) -> Bytes {
        self.carry.extend_from_slice(b);
        let Some(end) = self.carry.iter().rposition(|&c| c == b'\n') else { return Bytes::new() };
        let rest = self.carry.split_off(end + 1);
        let whole = std::mem::replace(&mut self.carry, rest);
        let mut out = Vec::with_capacity(whole.len());
        for l in whole.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
            let j = match serde_json::from_slice::<Json>(l) {
                Ok(j) => self.line(j),
                Err(e) => json!({"error": format!("a line of the feed is not JSON: {e}"), "status": 500}),
            };
            serde_json::to_writer(&mut out, &j).expect("JSON serialises");
            out.push(b'\n');
        }
        Bytes::from(out)
    }

    fn line(&self, mut j: Json) -> Json {
        if let Some(o) = j.as_object_mut() {
            if let Some(Json::Array(ops)) = o.get_mut("ops") {
                ops.retain(|op| op.get("table").and_then(Json::as_str).and_then(|t| self.who.table_scope(t)).is_some_and(|s| s >= Scope::Read));
            }
            o.remove("sources");
            o.remove("unknown");
        }
        j
    }
}

/// `POST /v1/feed/keys {"tables": [...], "slots"?: [...]}`: the keys of
/// `tables` as a follower's sweep compares them with its source's: with no
/// `slots`, `{"digests": [[slot, count, "sum in hex"], ...]}` of the slots
/// that hold one; with `slots`, `{"keys": [{"table", "key", "place"?},
/// ...]}` of the rows those place ([`crate::service::Service::feed_keys`];
/// a coordinator asks each shard's primary).
async fn feed_keys(State(app): State<App>, AxJson(b): AxJson<Json>) -> Result<AxJson<Json>, ApiError> {
    let names = |k: &str| -> Result<Vec<String>, ApiError> {
        b.get(k).and_then(Json::as_array).ok_or_else(|| ApiError::bad(format!("{k} is a list")))?.iter().map(|t| t.as_str().map(String::from).ok_or_else(|| ApiError::bad(format!("{k} lists names")))).collect()
    };
    let tables = names("tables")?;
    let slots = match b.get("slots") {
        None | Some(Json::Null) => None,
        Some(Json::Array(a)) => Some(a.iter().map(|s| s.as_u64().filter(|&s| (s as usize) < crate::slots::SLOTS).map(|s| s as usize).ok_or_else(|| ApiError::bad(format!("slots are 0 to {}", crate::slots::SLOTS - 1)))).collect::<Result<Vec<_>, _>>()?),
        Some(_) => return Err(ApiError::bad("slots is a list")),
    };
    let j = match app.svc.coordinator() {
        Ok(c) => c.feed_keys(tables, slots).await?,
        Err(_) => app.svc.feed_keys(tables, slots).await?,
    };
    Ok(AxJson(j))
}

/// The records of a log table ([`chloedb::rank_log::export`]) as NDJSON,
/// read from one snapshot of a local database as they are sent. A failure
/// after the first record ends the answer with `{"error": ..., "status":
/// ..}`. A coordinator runs the export's queries over its shards
/// ([`crate::export::rank_log`]), each read whole.
async fn export_rank_log(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let x = chloedb::rank_log::Export::from_json(&b)?;
    let p = who(w);
    rank_log_ok(&p, &x)?;
    // Refused before the answer starts.
    chloedb::rank_log::check(&*app.svc.table(&x.table).await?)?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Json>(1024);
    let Some(db) = app.svc.local_db().cloned() else {
        let (svc, owner) = (app.svc.clone(), crate::service::scroll_owner(&p));
        tokio::spawn(async move {
            let out = tx.clone();
            let r = crate::export::rank_log(&svc, &owner, x, move |r| out.blocking_send(r).map_err(|_| chloedb::Error::Query("the export's client left".into()))).await;
            if let Err(e) = r {
                let _ = tx.send(json!({"error": e.message, "status": e.status})).await;
            }
        });
        let lines = tokio_stream::wrappers::ReceiverStream::new(rx).map(|j| Ok::<_, std::convert::Infallible>(Bytes::from(format!("{j}\n"))));
        return Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], axum::body::Body::from_stream(lines)).into_response());
    };
    tokio::task::spawn_blocking(move || {
        let r = db.export_rank_log(&x, |r| tx.blocking_send(r).map_err(|_| chloedb::Error::Query("the export's client left".into())));
        if let Err(e) = r {
            let e = ApiError::from(e);
            let _ = tx.blocking_send(json!({"error": e.message, "status": e.status}));
        }
    });
    let lines = tokio_stream::wrappers::ReceiverStream::new(rx).map(|j| Ok::<_, std::convert::Infallible>(Bytes::from(format!("{j}\n"))));
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], axum::body::Body::from_stream(lines)).into_response())
}

/// Refuses an export of a rank log for a key limited to tables that may
/// not read the log's table or its events'.
fn rank_log_ok(who: &Principal, x: &chloedb::rank_log::Export) -> Result<(), ApiError> {
    who.may_table(Scope::Read, &x.table, "a rank log's export")?;
    if let Some(e) = &x.events {
        who.may_table(Scope::Read, &e.table, "a rank log's export (its events)")?;
    }
    Ok(())
}

/// The credit of each pair of interleaved rankings in a log table
/// ([`chloedb::rank_log::Credits`]) over an export of it (the body is
/// `/v1/rank_log/export`'s), from a local database as the export is.
async fn interleaving(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    let x = chloedb::rank_log::Export::from_json(&b)?;
    let p = who(w);
    rank_log_ok(&p, &x)?;
    chloedb::rank_log::check(&*app.svc.table(&x.table).await?)?;
    let t0 = std::time::Instant::now();
    let pairs = match app.svc.local_db().cloned() {
        Some(db) => tokio::task::spawn_blocking(move || db.interleaving_credit(&x)).await.map_err(ApiError::internal)??,
        // Credited over the export through the coordinator.
        None => {
            let credits = Arc::new(std::sync::Mutex::new(chloedb::rank_log::Credits::default()));
            let c = credits.clone();
            crate::export::rank_log(&app.svc, &crate::service::scroll_owner(&p), x, move |r| {
                c.lock().unwrap_or_else(|e| e.into_inner()).add(&r);
                Ok(())
            })
            .await?;
            let c = std::mem::take(&mut *credits.lock().unwrap_or_else(|e| e.into_inner()));
            c.finish()
        }
    };
    Ok(AxJson(json!({"pairs": pairs.iter().map(|c| c.to_json()).collect::<Vec<_>>(), "took_ms": t0.elapsed().as_secs_f64() * 1e3})))
}

/// A query's steps as a caller who is not the server's admin (a key of
/// read or write scope, or one limited to tables) sees them: a shard's step
/// says which shard, not the address of the copy that answered.
fn hide_hosts(who: &Principal, steps: &mut [chloedb::query::steps::Step]) {
    if who.scope == Scope::Admin && !who.limited() {
        return;
    }
    for s in steps.iter_mut().filter(|s| s.op == "shard") {
        if let Some(i) = s.detail.rfind(", ").filter(|&i| s.detail[i + 2..].contains("://")) {
            s.detail.truncate(i);
        }
    }
}

/// A query's answer; one a coordinator gave without some shards says
/// which (`"partial": {"missing_shards": [...]}`).
fn answer(a: &crate::service::Answer) -> Json {
    let mut j = js::result(&a.rs, a.took_ms);
    if !a.missing.is_empty() {
        j["partial"] = json!({"missing_shards": a.missing});
    }
    // A scroll's cursor, while rows are left to read.
    if let Some(c) = &a.cursor {
        j["cursor"] = json!(c);
    }
    j
}

/// `DELETE /v1/query/cursors/:id`: ends a scroll before its last page, for
/// the key that started it: `{"closed": true}`, or false when it was not
/// open (read to its end, closed, expired, or another key's).
async fn close_cursor(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    Ok(AxJson(json!({"closed": app.svc.close_scroll(&id, &crate::service::scroll_owner(&who(w)))})))
}

/// [`answer`], its rows as objects keyed by column when the body asks
/// (`"objects": true`; [`as_objects`]).
fn answer_of(b: &Json, a: &crate::service::Answer) -> Result<Json, ApiError> {
    let mut j = answer(a);
    // Why a MATCH of several words found nothing, or why score() is 0.
    if let Some(sql) = b.get("sql").and_then(|s| s.as_str()) {
        let zero = !a.rs.scores.is_empty() && a.rs.scores.iter().all(|s| *s == 0.0);
        if let Some(h) = chloedb::query::hint::of_answer(sql, a.rs.rows.len(), zero) {
            j["hint"] = json!(h);
        }
    }
    if !wants_steps(b) && !body_bool(b, "shard")? {
        without_steps(&mut j);
    }
    if body_bool(b, "objects")? {
        as_objects(&mut j);
    }
    Ok(j)
}

/// Whether a client asked for its answer's timed steps (`stats.steps`):
/// `"steps": true`, or `"profile": true`, which measures their CPU time.
/// Without, an answer leaves them out (they are most of its bytes), and
/// the list of queries (`/v1/queries`) keeps them all the same.
fn wants_steps(b: &Json) -> bool {
    ["steps", "profile"].iter().any(|k| b.get(*k).and_then(|v| v.as_bool()) == Some(true))
}

/// An answer without its timed steps ([`wants_steps`]).
fn without_steps(j: &mut Json) {
    if let Some(s) = j.get_mut("stats").and_then(Json::as_object_mut) {
        s.remove("steps");
        s.remove("steps_dropped");
    }
}

/// An answer's rows as objects, `{"id": "p1", "price": 9.5}`, in place of
/// lists in the order of its `columns`. A column named as an earlier one
/// (`SELECT a.id, b.id`) is keyed by its name and its place among those of
/// its name: `id`, then `id_2`, `id_3` (or the next suffix no column has).
pub fn as_objects(j: &mut Json) {
    let mut names: Vec<String> = Vec::new();
    for c in j["columns"].as_array().into_iter().flatten() {
        let c = c.as_str().unwrap_or_default();
        let mut name = c.to_string();
        let mut k = 1;
        while names.contains(&name) {
            k += 1;
            name = format!("{c}_{k}");
        }
        names.push(name);
    }
    for r in j.get_mut("rows").and_then(Json::as_array_mut).into_iter().flatten() {
        if let Json::Array(values) = std::mem::take(r) {
            *r = Json::Object(names.iter().cloned().zip(values).collect());
        }
    }
}

/// Any statement: the service checks that the request's credential may
/// run it ([`Service::sql`]), and audits a table's or rank profile's.
async fn sql(State(app): State<App>, w: Option<axum::Extension<Principal>>, headers: HeaderMap, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let w = who(w);
    let params = app.svc.params_for(b.get("params"), b.get("sql").and_then(Json::as_str), &w).await?;
    if let Some(log) = b.get("log") {
        let req = QueryReq { sql: body_str(&b, "sql")?.to_string(), params, timeout_ms: body_u64(&b, "timeout_ms")?, after: after(&b)?, who: Some(w.clone()), ..Default::default() };
        return logged(&app, &w, req, log, body_bool(&b, "objects")?).await.map(IntoResponse::into_response);
    }
    let statement = body_str(&b, "sql")?;
    // An agent does not drop a table, or delete every row, alone
    // ([`crate::changes::sql_tier`]): it files a change request.
    if crate::ops::is_agent(&w, keys::caller().as_ref()) {
        let (tier, _) = crate::changes::sql_tier(statement, &app.svc.tables_listed().await);
        if tier >= 3 {
            return Err(ApiError::coded(403, "change_request_required", "an agent does not run this statement alone (it drops a table, a field, a rank profile or a function, or deletes or updates every row): dry run it (POST /v1/admin/changes/preview) and file a change request (POST /v1/admin/changes) for a person to approve"));
        }
    }
    // A write from the command center is audited whatever its answer.
    let writes = table_written(statement).is_some();
    if writes {
        audit_ui_write(statement, None, None);
    }
    let key = body_key(&b, &headers)?;
    let (r, partial) = crate::coord::with_partial(app.svc.sql_budgeted(&w, statement, &params, after(&b)?, body_u64(&b, "timeout_ms")?, None, key, body_budget(&b)?)).await;
    match r {
        Ok(SqlAnswer::Rows(mut a)) => {
            hide_hosts(&w, &mut a.rs.stats.steps);
            Ok(AxJson(answer_of(&b, &a)?).into_response())
        }
        Ok(SqlAnswer::Written { w, took_ms }) => {
            audit_ui_write(statement, Some(w.rows as u64), None);
            Ok(AxJson(written(w, took_ms)).into_response())
        }
        Err(e) => {
            if writes {
                audit_ui_write(statement, None, partial.as_ref().map(|p| &p["shards"]));
            }
            Ok(write_failed(e, partial))
        }
    }
}

/// A write's idempotency key: the body's `idempotency_key`, or an
/// `Idempotency-Key` header.
fn body_key(b: &Json, headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let key = match (b.get("idempotency_key"), headers.get("idempotency-key")) {
        (Some(Json::String(k)), _) => Some(k.clone()),
        (Some(_), _) => return Err(ApiError::bad("idempotency_key is a string")),
        (None, Some(h)) => Some(h.to_str().map_err(|_| ApiError::bad("the Idempotency-Key header is visible ASCII alone (letters, digits, - and the like): send non-ASCII text percent-encoded, or a UUID"))?.to_string()),
        (None, None) => None,
    };
    if key.as_ref().is_some_and(|k| k.is_empty() || k.len() > 256) {
        return Err(ApiError::bad("an idempotency key has 1 to 256 characters"));
    }
    Ok(key)
}

/// The answer of a write that failed: its error, and when it failed
/// partway through a coordinator, what each shard did (`shards`: `{shard,
/// short, rows}` for those that took it, `{shard, short, error}` for the
/// others) and the idempotency key to send it again with
/// (`idempotency_key`; [`crate::coord::with_partial`]).
fn write_failed(e: ApiError, partial: Option<Json>) -> Response {
    let Some(partial) = partial else { return e.into_response() };
    // The error's shape (`error_code`, `retryable`, `hint`), with what was
    // written: whether to send it again matters most here.
    let mut body = crate::errors::answer(&e);
    body["partial"] = json!(true);
    body["shards"] = partial["shards"].clone();
    if let Some(k) = partial.get("idempotency_key") {
        body["idempotency_key"] = k.clone();
    }
    let body = body.to_string();
    let mut r = e.into_response();
    *r.body_mut() = axum::body::Body::from(body);
    r
}

/// A write the command center sent (`x-strata-client: command-center`),
/// noted for the audit log as a change whatever its answer: its object
/// (`table products`), what was sent (a statement's first 2 KB, or what
/// the request was), its table, the rows written once known, and when it
/// failed partway each shard's part.
fn audit_ui(object: Option<String>, sent: &str, table: Option<&str>, rows: Option<u64>, shards: Option<&Json>) {
    if !keys::caller().is_some_and(|c| c.command_center()) {
        return;
    }
    let mut end = sent.len().min(2048);
    while !sent.is_char_boundary(end) {
        end -= 1;
    }
    let mut after = json!({"statement": &sent[..end], "table": table, "rows": rows});
    if let Some(s) = shards {
        after["shards"] = s.clone();
    }
    keys::note_change(keys::Change { object, after: Some(after), ..Default::default() });
    let of = table.map_or(String::new(), |t| format!(" of {t}"));
    let sent: String = sent.chars().take(200).collect();
    keys::note(match (rows, shards) {
        (Some(n), _) => format!("wrote {n} rows{of}: {sent}"),
        (None, Some(_)) => format!("a write{of} that failed partway: {sent}"),
        (None, None) => format!("a write{of}: {sent}"),
    });
}

/// [`audit_ui`] of a statement: its table is the one it names.
fn audit_ui_write(statement: &str, rows: Option<u64>, shards: Option<&Json>) {
    let table = table_written(statement);
    // workdata (NEW-data-tools-12): a table's statement counts no rows.
    let ddl = statement.split_whitespace().next().is_some_and(|w| ["CREATE", "DROP", "ALTER", "TRUNCATE"].iter().any(|k| w.eq_ignore_ascii_case(k)));
    audit_ui(table.as_ref().map(|t| format!("table {t}")), statement, table.as_deref(), rows.filter(|_| !ddl), shards);
    if ddl && rows.is_some() && keys::caller().is_some_and(|c| c.command_center()) {
        keys::note(format!("ran: {}", statement.chars().take(200).collect::<String>()));
    }
}

/// The table a statement writes: what follows INSERT INTO, UPDATE, DELETE
/// FROM, or a table's CREATE, DROP or ALTER.
fn table_written(sql: &str) -> Option<String> {
    let words: Vec<&str> = sql.split_whitespace().take(6).collect();
    let up: Vec<String> = words.iter().map(|w| w.to_ascii_uppercase()).collect();
    let at = match up.first().map(String::as_str)? {
        "INSERT" | "REPLACE" | "UPSERT" => up.iter().position(|w| w == "INTO")? + 1,
        "UPDATE" => 1,
        "DELETE" => up.iter().position(|w| w == "FROM")? + 1,
        "CREATE" | "DROP" | "ALTER" | "TRUNCATE" => {
            let t = up.iter().position(|w| w == "TABLE")? + 1;
            t + up[t..].iter().take_while(|w| matches!(w.as_str(), "IF" | "NOT" | "EXISTS")).count()
        }
        _ => return None,
    };
    let name: String = words.get(at)?.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.').collect();
    (!name.is_empty()).then_some(name)
}

/// A write's answer: the rows written, and the transaction's sequence
/// number in a database that numbers its writes, or through a
/// coordinator each shard's ([`Written`]).
fn written(w: Written, took_ms: f64) -> Json {
    let mut j = json!({"written": w.rows, "took_ms": took_ms});
    if let Some(r) = receipt_json(&w) {
        j["receipt"] = r;
    }
    if w.skipped > 0 {
        j["skipped"] = json!(w.skipped);
    }
    if let Some(s) = w.seq_no {
        j["seq_no"] = json!(s);
    }
    if !w.seq_nos.is_empty() {
        j["seq_nos"] = json!(w.seq_nos);
    }
    j
}

/// What a write's operations found ([`crate::service::Receipt`]):
/// `{received, inserted, replaced, updated, deleted, collapsed, not_found,
/// per_shard}`, the counts null where a shard did not say (an older one).
/// None for a write without operations (a SQL statement on one server).
fn receipt_json(w: &Written) -> Option<Json> {
    if w.receipt.is_none() && w.shards.is_empty() && w.received == 0 {
        return None;
    }
    let counts = |r: Option<crate::service::Receipt>| match r {
        Some(r) => serde_json::to_value(r).unwrap_or_default(),
        None => json!({"inserted": null, "replaced": null, "updated": null, "deleted": null, "collapsed": null, "not_found": null}),
    };
    let mut j = counts(w.receipt);
    j["received"] = json!(w.received);
    if !w.shards.is_empty() {
        j["per_shard"] = Json::Array(
            w.shards
                .iter()
                .map(|p| {
                    let mut s = counts(p.receipt);
                    s["shard"] = json!(p.shard);
                    s["rows"] = json!(p.rows);
                    s
                })
                .collect(),
        );
    }
    Some(j)
}

/// `strict` (a body's or a query's): each operation is sent with
/// `"strict": true`, which refuses, writing nothing, one that would
/// replace a row, write a key an earlier one wrote, or find no row.
fn strict_ops(ops: &mut [Json]) {
    for op in ops.iter_mut().filter(|op| !crate::service::is_mark(op)) {
        if op.get("strict").is_none() {
            op["strict"] = json!(true);
        }
    }
}

/// Runs a plan ([`chloedb::query::ir`]) as `/v1/sql` runs a statement.
async fn run_plan(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    let plan = chloedb::query::ir::Plan::from_json(b.get("plan").ok_or_else(|| ApiError::bad("the body needs a \"plan\""))?)?;
    let who = who(w);
    match app.svc.run_plan(&who, plan, after(&b)?, body_u64(&b, "timeout_ms")?).await? {
        SqlAnswer::Rows(mut a) => {
            hide_hosts(&who, &mut a.rs.stats.steps);
            Ok(AxJson(answer_of(&b, &a)?))
        }
        SqlAnswer::Written { w, took_ms } => Ok(AxJson(written(w, took_ms))),
    }
}

/// The body's `ops`, applied once per idempotency key (the body's
/// `idempotency_key` or an `Idempotency-Key` header).
async fn write(State(app): State<App>, w: Option<axum::Extension<Principal>>, headers: HeaderMap, AxJson(mut b): AxJson<Json>) -> Result<Response, ApiError> {
    let mut ops = match b.get_mut("ops").map(Json::take) {
        Some(Json::Array(ops)) => ops,
        _ => return Err(ApiError::bad("the body needs an array \"ops\"")),
    };
    match b.get("strict") {
        None | Some(Json::Null) | Some(Json::Bool(false)) => {}
        Some(Json::Bool(true)) => strict_ops(&mut ops),
        Some(_) => return Err(ApiError::bad("strict is true or false")),
    }
    let who = who(w);
    if who.limited() {
        // Each operation's table, or the default table for one that names
        // none.
        let mut seen = std::collections::BTreeSet::new();
        for op in &ops {
            // A source's offset is the stream's, whatever table the op
            // writes: a key limited to tables could stop another table's
            // stream by marking its offset far ahead.
            if has_source(op) || is_mark(op) {
                return Err(who.may_server("an operation with a \"source\" (a stream's offset)").unwrap_err());
            }
            let table = match &op["table"] {
                Json::Null => None,
                Json::String(t) => Some(t.as_str()),
                _ => return Err(ApiError::bad("an operation's table is a table's name")),
            };
            if seen.insert(table.map(str::to_ascii_lowercase)) {
                table_ok(&app, &who, table, Scope::Write, "a write").await?;
            }
        }
    }
    let key = body_key(&b, &headers)?;
    let t0 = std::time::Instant::now();
    // A write from the command center is audited whatever its answer: its
    // table is its first operation's.
    let shown = keys::caller().is_some_and(|c| c.command_center()).then(|| {
        let table = ops.iter().find_map(|op| op["table"].as_str().map(String::from)).unwrap_or_else(|| app.svc.default_table().map_or_else(|_| app.svc.first_table().table.clone(), |t| t.table.clone()));
        (Json::Array(ops.clone()).to_string(), table)
    });
    let note = |rows: Option<u64>, shards: Option<&Json>| {
        if let Some((ops, table)) = &shown {
            audit_ui(Some(format!("table {table}")), ops, Some(table), rows, shards);
        }
    };
    note(None, None);
    let (r, partial) = crate::coord::with_partial(app.svc.write_request(ops, key)).await;
    match r {
        Ok(w) => {
            note(Some(w.rows as u64), None);
            Ok(AxJson(written(w, t0.elapsed().as_secs_f64() * 1e3)).into_response())
        }
        Err(e) => {
            note(None, partial.as_ref().map(|p| &p["shards"]));
            Ok(write_failed(e, partial))
        }
    }
}

/// The writes a read by key must see, from its query string
/// (`min_seq_no=n`, or `min_seq_nos=a,b,c`).
fn after_of_query(q: &std::collections::HashMap<String, String>) -> Result<After, ApiError> {
    let mut b = json!({});
    if let Some(v) = q.get("min_seq_no") {
        b["min_seq_no"] = json!(v.parse::<u64>().map_err(|_| ApiError::bad("min_seq_no is a sequence number: a whole number from 0"))?);
    }
    if let Some(v) = q.get("min_seq_nos") {
        let ns: Vec<u64> = v.split(',').map(|x| x.trim().parse::<u64>()).collect::<Result<_, _>>().map_err(|_| ApiError::bad("min_seq_nos is sequence numbers joined by commas, one per shard"))?;
        b["min_seq_nos"] = json!(ns);
    }
    after(&b)
}

/// A row read by key as JSON: an object of its columns.
fn key_row(columns: &[String], row: &[chloedb::value::Value]) -> Json {
    Json::Object(columns.iter().zip(row).map(|(c, v)| (c.clone(), js::to_json(v))).collect())
}

/// The key in a path as the table's key: an INT key's digits a number.
fn path_key(schema: &chloedb::schema::Schema, key: &str) -> Result<Json, ApiError> {
    let pk = schema.primary_key.ok_or_else(|| ApiError::bad(format!("{} has no primary key: rows are read and written by key only in a table with one", schema.table)))?;
    Ok(match schema.fields[pk].ty {
        chloedb::schema::FieldType::Int => json!(key.parse::<i64>().map_err(|_| ApiError::bad(format!("{key:?} is not a key of {}: its key {} is an INT", schema.table, schema.fields[pk].name)))?),
        _ => json!(key),
    })
}

/// `GET /v1/kv/:table/:key`: one row by its key, without SQL.
async fn kv_one(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path((table, key)): axum::extract::Path<(String, String)>, Query(q): Query<std::collections::HashMap<String, String>>) -> Result<Response, ApiError> {
    table_ok(&app, &who(w), Some(&table), Scope::Read, "a read by key").await?;
    let t0 = Instant::now();
    let timeout_ms = q.get("timeout_ms").and_then(|v| v.parse().ok()).unwrap_or(0);
    // The key as the table's key is typed: an INT key's text that is not
    // a number says so, not that a query is invalid.
    let k = js::param(&path_key(&*app.svc.table(&table).await?, &key)?)?;
    let got = app.svc.get_rows(Some(&table), vec![k], None, &after_of_query(&q)?, timeout_ms, true).await?;
    let took_ms = t0.elapsed().as_secs_f64() * 1e3;
    Ok(match got.rows.into_iter().next().flatten() {
        Some((row, seq_no)) => AxJson(json!({"table": got.table, "key": key, "found": true, "row": key_row(&got.columns, &row), "seq_no": seq_no, "took_ms": took_ms})).into_response(),
        None => (StatusCode::NOT_FOUND, AxJson(json!({"table": got.table, "key": key, "found": false, "took_ms": took_ms}))).into_response(),
    })
}

/// `POST /v1/kv/get`: rows by their keys (MGET), without SQL.
async fn kv_get(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let t0 = Instant::now();
    let keys = match b.get("keys") {
        Some(Json::Array(k)) => k.iter().map(js::param).collect::<Result<Vec<_>, _>>()?,
        _ => return Err(ApiError::bad("the body needs an array \"keys\"")),
    };
    let table = match b.get("table") {
        None | Some(Json::Null) => None,
        Some(Json::String(t)) => Some(t.as_str()),
        Some(_) => return Err(ApiError::bad("table is a table's name")),
    };
    let columns = match b.get("columns") {
        None | Some(Json::Null) => None,
        Some(Json::Array(c)) => Some(c.iter().map(|c| c.as_str().map(str::to_string)).collect::<Option<Vec<_>>>().ok_or_else(|| ApiError::bad("columns are the names of columns"))?),
        Some(_) => return Err(ApiError::bad("columns is an array of the names of columns")),
    };
    table_ok(&app, &who(w), table, Scope::Read, "a read by key").await?;
    let got = app.svc.get_rows(table, keys, columns, &after(&b)?, body_u64(&b, "timeout_ms")?, true).await?;
    Ok(AxJson(KeyRowsJson { got: &got, took_ms: t0.elapsed().as_secs_f64() * 1e3 }).into_response())
}

/// A read by key's answer ([`kv_get`]), written as it is read rather than
/// built as JSON first: `{"columns", "rows", "seq_nos", "table",
/// "took_ms"}`, a row an object of its columns in the order of their
/// names (as JSON objects here are), a key not found `null` in both lists.
struct KeyRowsJson<'a> {
    got: &'a crate::service::KeyRows,
    took_ms: f64,
}

impl serde::Serialize for KeyRowsJson<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        struct Row<'a>(&'a [String], &'a [usize], &'a [chloedb::value::Value]);
        impl serde::Serialize for Row<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut m = s.serialize_map(None)?;
                // A name twice: the last of its columns the row has.
                let mut last: Option<&str> = None;
                for &i in self.1 {
                    let Some(v) = self.2.get(i) else { continue };
                    if last == Some(self.0[i].as_str()) {
                        continue;
                    }
                    last = Some(&self.0[i]);
                    m.serialize_entry(&self.0[i], &js::AsJson(v))?;
                }
                m.end()
            }
        }
        struct Rows<'a>(&'a crate::service::KeyRows, &'a [usize]);
        impl serde::Serialize for Rows<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut q = s.serialize_seq(Some(self.0.rows.len()))?;
                for r in &self.0.rows {
                    q.serialize_element(&r.as_ref().map(|(row, _)| Row(&self.0.columns, self.1, row)))?;
                }
                q.end()
            }
        }
        let got = self.got;
        // The columns in the order of their names, a name's last first.
        let mut by_name: Vec<usize> = (0..got.columns.len()).collect();
        by_name.sort_by(|&a, &b| got.columns[a].cmp(&got.columns[b]).then(b.cmp(&a)));
        let mut m = s.serialize_map(Some(5))?;
        m.serialize_entry("columns", &got.columns)?;
        m.serialize_entry("rows", &Rows(got, &by_name))?;
        m.serialize_entry("seq_nos", &got.rows.iter().map(|r| r.as_ref().map(|(_, n)| *n)).collect::<Vec<_>>())?;
        m.serialize_entry("table", &got.table)?;
        m.serialize_entry("took_ms", &self.took_ms)?;
        m.end()
    }
}

/// The idempotency key of a write: the `Idempotency-Key` header.
fn idempotency(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let key = headers.get("idempotency-key").map(|h| h.to_str().map(str::to_string).map_err(|_| ApiError::bad("the Idempotency-Key header is visible ASCII alone (letters, digits, - and the like): send non-ASCII text percent-encoded, or a UUID"))).transpose()?;
    if key.as_ref().is_some_and(|k| k.is_empty() || k.len() > 256) {
        return Err(ApiError::bad("an idempotency key has 1 to 256 characters"));
    }
    Ok(key)
}

/// `if_seq_no` from a query string, for a write by key.
fn if_seq_no(op: &mut Json, q: &std::collections::HashMap<String, String>) -> Result<(), ApiError> {
    if let Some(v) = q.get("if_seq_no") {
        op["if_seq_no"] = json!(v.parse::<u64>().map_err(|_| ApiError::bad("if_seq_no is a sequence number: a whole number from 0"))?);
    }
    Ok(())
}

/// `PUT /v1/kv/:table/:key`: writes the row with that key (an upsert),
/// without SQL.
async fn kv_put(
    State(app): State<App>,
    w: Option<axum::Extension<Principal>>,
    axum::extract::Path((table, key)): axum::extract::Path<(String, String)>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    AxJson(mut doc): AxJson<Json>,
) -> Res {
    table_ok(&app, &who(w), Some(&table), Scope::Write, "a write by key").await?;
    let t0 = Instant::now();
    let schema = app.svc.table(&table).await?;
    let k = path_key(&schema, &key)?;
    let pk = schema.fields[schema.primary_key.expect("path_key checked it")].name.clone();
    let Json::Object(m) = &mut doc else { return Err(ApiError::bad("the body is the row's fields, as an object")) };
    match m.iter().find(|(f, _)| f.eq_ignore_ascii_case(&pk)) {
        Some((_, v)) if *v != k => return Err(ApiError::bad(format!("the body's {pk} is {v}, and the path's key {k}"))),
        Some(_) => {}
        None => {
            m.insert(pk, k);
        }
    }
    let mut op = json!({"op": "upsert", "table": schema.table, "doc": doc});
    if_seq_no(&mut op, &q)?;
    let w = app.svc.write_ops(vec![op], idempotency(&headers)?).await?;
    Ok(AxJson(written(w, t0.elapsed().as_secs_f64() * 1e3)))
}

/// `DELETE /v1/kv/:table/:key`: deletes the row with that key.
async fn kv_delete(
    State(app): State<App>,
    w: Option<axum::Extension<Principal>>,
    axum::extract::Path((table, key)): axum::extract::Path<(String, String)>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Res {
    table_ok(&app, &who(w), Some(&table), Scope::Write, "a delete by key").await?;
    let t0 = Instant::now();
    let schema = app.svc.table(&table).await?;
    let mut op = json!({"op": "delete", "table": schema.table, "key": path_key(&schema, &key)?});
    if_seq_no(&mut op, &q)?;
    if let Some(r) = q.get("routing") {
        op["routing"] = json!(r);
    }
    let w = app.svc.write_ops(vec![op], idempotency(&headers)?).await?;
    let mut j = written(w, t0.elapsed().as_secs_f64() * 1e3);
    j["deleted"] = json!(j["written"].as_u64().unwrap_or(0) > 0);
    Ok(AxJson(j))
}

/// Rows in the body, in the format `?format=` names or the content type
/// implies. The content type must not be one a page on another site may
/// send without asking first (text/plain, a form, or none).
async fn import(State(app): State<App>, w: Option<axum::Extension<Principal>>, Query(q): Query<std::collections::HashMap<String, String>>, headers: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
    let ui = keys::caller().is_some_and(|c| c.command_center()) && !flag(&q, "plan", false).unwrap_or(false);
    if !ui {
        return import_rows(app, w, q, headers, body).await.map(IntoResponse::into_response);
    }
    // From the command center, audited whatever its answer: the table it
    // names, else a SQLite file's tables, else the first table.
    let (object, table) = match (q.get("table"), q.get("tables")) {
        (Some(t), _) => (format!("table {t}"), Some(t.clone())),
        (None, Some(t)) => (format!("tables {t}"), None),
        (None, None) if body.starts_with(chloedb::sqlite::MAGIC) => ("the tables of a SQLite file".to_string(), None),
        (None, None) => {
            let t = app.svc.default_table().map_or_else(|_| app.svc.first_table().table.clone(), |t| t.table.clone());
            (format!("table {t}"), Some(t))
        }
    };
    let sent = format!("an import of {} bytes{}", body.len(), q.get("format").map_or(String::new(), |f| format!(" of {f}")));
    let note = |rows: Option<u64>, shards: Option<&Json>| audit_ui(Some(object.clone()), &sent, table.as_deref(), rows, shards);
    note(None, None);
    let (r, partial) = crate::coord::with_partial(import_rows(app, w, q, headers, body)).await;
    match r {
        Ok(AxJson(j)) => {
            note(j["written"].as_u64(), None);
            Ok(AxJson(j).into_response())
        }
        Err(e) => {
            note(None, partial.as_ref().map(|p| &p["shards"]));
            Ok(write_failed(e, partial))
        }
    }
}

/// `POST /v1/import` ([`import`]).
async fn import_rows(app: App, w: Option<axum::Extension<Principal>>, q: std::collections::HashMap<String, String>, headers: HeaderMap, body: Bytes) -> Res {
    if auth::simple_content_type(header_str(&headers, header::CONTENT_TYPE)) {
        return Err(ApiError {
            code: None, status: 415,
            message: "send the rows with their content type (text/csv, text/tab-separated-values, application/x-ndjson, application/json, application/vnd.sqlite3, application/vnd.apache.parquet or application/octet-stream), not text/plain or a form".into(),
        });
    }
    let ct = header_str(&headers, header::CONTENT_TYPE).unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let sqlite = match q.get("format") {
        Some(f) => f.eq_ignore_ascii_case("sqlite"),
        None => matches!(ct.as_str(), "application/vnd.sqlite3" | "application/x-sqlite3") || body.starts_with(chloedb::sqlite::MAGIC),
    };
    if sqlite {
        return import_sqlite(&app, &who(w.clone()), &q, body).await;
    }
    let parquet = match q.get("format") {
        Some(f) => f.eq_ignore_ascii_case("parquet"),
        None => matches!(ct.as_str(), "application/vnd.apache.parquet" | "application/x-parquet") || body.starts_with(b"PAR1"),
    };
    if parquet {
        return import_parquet(&app, &who(w.clone()), &q, body).await;
    }
    let format = match q.get("format") {
        Some(f) => Some(Format::parse(f).ok_or_else(|| ApiError::bad(format!("unknown format {f:?}: jsonl, json, csv, tsv, sqlite or parquet")))?),
        None => {
            let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
            let ct = ct.split(';').next().unwrap_or("").trim();
            match ct {
                "text/csv" => Some(Format::Csv),
                "text/tab-separated-values" => Some(Format::Tsv),
                "application/x-ndjson" | "application/jsonl" | "application/x-jsonlines" => Some(Format::Jsonl),
                "application/json" => Some(Format::Json),
                _ => None,
            }
        }
    };
    let format = format.ok_or_else(|| ApiError::bad("name the rows' format: ?format=jsonl, json, csv, tsv, sqlite or parquet"))?;
    table_ok(&app, &who(w), q.get("table").map(String::as_str), Scope::Write, "an import").await?;
    let t0 = std::time::Instant::now();
    let empty = match q.get("empty") {
        None => chloedb::ingest::EmptyCell::Default,
        Some(e) => chloedb::ingest::EmptyCell::parse(e).ok_or_else(|| ApiError::bad(format!("empty is default, null or text, not {e:?}")))?,
    };
    let opts = crate::service::ImportOpts { strict: flag(&q, "strict", false)?, empty };
    let (w, skipped, ragged) = app.svc.import(q.get("table").map(String::as_str), body, format, opts).await?;
    let mut j = json!({"written": w.rows, "skipped_fields": skipped, "took_ms": t0.elapsed().as_secs_f64() * 1e3});
    // Rows of a CSV or TSV file with more or fewer values than its header
    // (?strict=true refuses them).
    if ragged > 0 {
        j["ragged_rows"] = json!(ragged);
    }
    if let Some(r) = receipt_json(&w) {
        j["receipt"] = r;
    }
    Ok(AxJson(j))
}

/// A query parameter that is true or false (`1`, `0`, `yes`, `no`), or
/// `default` when it is missing.
fn flag(q: &std::collections::HashMap<String, String>, k: &str, default: bool) -> Result<bool, ApiError> {
    match q.get(k).map(|v| v.to_ascii_lowercase()) {
        None => Ok(default),
        Some(v) => match v.as_str() {
            "" | "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            _ => Err(ApiError::bad(format!("{k} is true or false, not {v:?}"))),
        },
    }
}

/// `?tables=a,b`: the tables named, or none for every table.
fn tables_param(q: &std::collections::HashMap<String, String>) -> Vec<String> {
    q.get("tables").map(|t| t.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect()).unwrap_or_default()
}

/// A SQLite file's tables ([`Service::import_sqlite`]): `?tables=a,b`
/// imports only those, `?blobs_base64=true` keeps BLOB columns as base64
/// text, and `?plan=true` answers what the import would do and writes
/// nothing.
async fn import_sqlite(app: &App, who: &Principal, q: &std::collections::HashMap<String, String>, body: Bytes) -> Res {
    if q.contains_key("table") {
        return Err(ApiError::bad("table is for rows: a SQLite file's tables go to the tables of their names (?tables= picks them)"));
    }
    let opts = chloedb::sqlite::ImportOptions { tables: tables_param(q), schemas: Vec::new(), blobs_base64: flag(q, "blobs_base64", false)? };
    let plan = flag(q, "plan", false)?;
    let t0 = std::time::Instant::now();
    let mut j = app.svc.import_sqlite(who, body, opts, plan, 0).await?;
    if !plan {
        let written: u64 = j["tables"].as_array().map_or(0, |t| t.iter().filter_map(|t| t["rows"].as_u64()).sum());
        j["written"] = json!(written);
        j["took_ms"] = json!(t0.elapsed().as_secs_f64() * 1e3);
    }
    Ok(AxJson(j))
}

/// A Parquet file's rows, into one table ([`Service::import_parquet`]):
/// `?table=` names it (else the table an export kept in the file),
/// `?key=` the key of a table the import creates (`""` for none),
/// `?blobs_base64=true` keeps binary columns as base64 text, and
/// `?plan=true` answers what the import would do and writes nothing.
#[cfg(feature = "parquet")]
async fn import_parquet(app: &App, who: &Principal, q: &std::collections::HashMap<String, String>, body: Bytes) -> Res {
    if q.contains_key("tables") {
        return Err(ApiError::bad("tables is for SQLite files: a Parquet file's rows go to one table (?table=)"));
    }
    let opts = chloedb::parquet::ImportOptions { table: q.get("table").cloned(), schema: None, key: q.get("key").cloned(), blobs_base64: flag(q, "blobs_base64", false)? };
    let plan = flag(q, "plan", false)?;
    let t0 = std::time::Instant::now();
    let mut j = app.svc.import_parquet(who, body, opts, plan, 0).await?;
    if !plan {
        j["written"] = j["rows"].clone();
        j["took_ms"] = json!(t0.elapsed().as_secs_f64() * 1e3);
    }
    Ok(AxJson(j))
}

#[cfg(not(feature = "parquet"))]
async fn import_parquet(_: &App, _: &Principal, _: &std::collections::HashMap<String, String>, _: Bytes) -> Res {
    Err(ApiError::bad("this server was built without Parquet (feature parquet)"))
}

/// The database as a new SQLite file ([`Service::export_sqlite`]), sent
/// as it is read from disk: `?tables=a,b` for only those tables,
/// `?fts=false`, `?indexes=false` and `?schemas=false` to leave out the
/// FTS5 indexes, the B-tree indexes and the kept Strata schemas. With
/// `?format=parquet`, one table (`?tables=`, which a database of one
/// table may leave out) as a Parquet file ([`Service::export_parquet`]),
/// `?schemas=false` leaving out its kept Strata schema.
///
/// `?format=jsonl`, `csv` or `arrow`, and on a coordinator `parquet` too,
/// answer one table's rows read by a scroll ([`crate::export`]), which a
/// key with read on that table may ask for; the database's files (SQLite,
/// a server's own Parquet) are the admin's.
async fn export(State(app): State<App>, w: Option<axum::Extension<Principal>>, Query(q): Query<std::collections::HashMap<String, String>>) -> Result<Response, ApiError> {
    let format = q.get("format").map(|f| f.to_ascii_lowercase());
    let who = who(w);
    let cluster = app.svc.coordinator().is_ok();
    let rows = match format.as_deref() {
        Some("parquet") if !cluster => None,
        Some(f) => crate::export::Format::parse(f),
        None => None,
    };
    if let Some(f) = rows {
        let mut tables = tables_param(&q);
        let table = match (tables.pop(), tables.is_empty()) {
            (Some(t), true) => t,
            _ => return Err(ApiError::bad("an export of rows is of one table: name it (?tables=name)")),
        };
        table_ok(&app, &who, Some(&table), Scope::Read, "an export").await?;
        who.may(Scope::Read, "an export")?;
        return crate::export::rows(&app.svc, &crate::service::scroll_owner(&who), &table, f).await;
    }
    if who.limited() {
        return Err(ApiError { code: None, status: 403, message: "an export of the database's files (format sqlite or parquet) is not for a key limited to tables: format jsonl, csv or arrow export a table's rows with read on it".into() });
    }
    who.may(Scope::Admin, "an export of the database's files (format sqlite or parquet; format jsonl, csv or arrow export a table's rows with read on it)")?;
    match format.as_deref() {
        None | Some("sqlite") => {}
        Some("parquet") => return export_parquet(&app, &q).await,
        Some(f) => return Err(ApiError::bad(format!("unknown format {f:?}: format=sqlite, parquet, jsonl, csv or arrow"))),
    }
    let opts = chloedb::sqlite::ExportOptions { tables: tables_param(&q), fts: flag(&q, "fts", true)?, indexes: flag(&q, "indexes", true)?, meta: flag(&q, "schemas", true)? };
    let (dir, path, done) = app.svc.export_sqlite(opts).await?;
    let name = done.tables.first().map_or("chloedb".into(), |t| t.table.clone());
    let tables: Vec<String> = done.tables.iter().map(|t| t.table.clone()).collect();
    send_file(dir, &path, "application/vnd.sqlite3", &format!("{name}.sqlite"), &tables).await
}

#[cfg(feature = "parquet")]
async fn export_parquet(app: &App, q: &std::collections::HashMap<String, String>) -> Result<Response, ApiError> {
    let mut tables = tables_param(q);
    if tables.len() > 1 {
        return Err(ApiError::bad("a Parquet file holds one table: name one (?tables=name), or export to a directory with chloedb-server export"));
    }
    let (dir, path, done) = app.svc.export_parquet(tables.pop(), flag(q, "schemas", true)?).await?;
    let tables: Vec<String> = done.tables.iter().map(|t| t.table.clone()).collect();
    let name = tables.first().cloned().unwrap_or_else(|| "chloedb".into());
    send_file(dir, &path, "application/vnd.apache.parquet", &format!("{name}.parquet"), &tables).await
}

#[cfg(not(feature = "parquet"))]
async fn export_parquet(_: &App, _: &std::collections::HashMap<String, String>) -> Result<Response, ApiError> {
    Err(ApiError::bad("this server was built without Parquet (feature parquet)"))
}

/// The file `path` as the answer, read from disk as it is sent; `dir`,
/// which holds it, goes when the last chunk is sent.
async fn send_file(dir: tempfile::TempDir, path: &std::path::Path, content_type: &str, filename: &str, tables: &[String]) -> Result<Response, ApiError> {
    let file = tokio::fs::File::open(path).await.map_err(ApiError::internal)?;
    let len = file.metadata().await.map_err(ApiError::internal)?.len();
    // The directory goes when the last chunk is sent.
    let chunks = futures::stream::unfold((file, Some(dir)), |(mut file, dir)| async move {
        use tokio::io::AsyncReadExt;
        dir.as_ref()?;
        let mut buf = vec![0u8; 1 << 16];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), (file, dir)))
            }
            Err(e) => Some((Err(e), (file, None))),
        }
    });
    let [new, old] = crate::names::both("tables");
    Ok((
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\"")),
            (new, tables.join(",")),
            (old, tables.join(",")),
        ],
        axum::body::Body::from_stream(chunks),
    )
        .into_response())
}

/// Write operations as NDJSON: one a line, or one event's several as
/// `{"ops": [...], "source": {...}}` (the source going to each operation
/// that names none), which a batch never splits. They are written in
/// batches as they arrive ([`Service::stream_write`]), and each batch is
/// answered by a line once it commits: `{"batch": n, "ops": .., "written":
/// .., "skipped": .., "lines": ..}`, `lines` counting the lines committed
/// so far. The answer ends with `{"done": true, ...}`, or with `{"error":
/// ..., "status": .., "lines": ..}` at the first failure, nothing after
/// the lines it counts applied.
async fn stream(State(app): State<App>, w: Option<axum::Extension<Principal>>, headers: HeaderMap, body: axum::body::Body) -> Result<Response, ApiError> {
    if auth::simple_content_type(header_str(&headers, header::CONTENT_TYPE)) {
        return Err(ApiError { code: None, status: 415, message: "send the operations as application/x-ndjson, not text/plain or a form".into() });
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Json>();
    // From the command center, audited once it ends: what it wrote.
    let ui = keys::caller().filter(|c| c.command_center()).map(|c| (c, who(w), obs::current().map(|id| id.to_string())));
    let svc = app.svc.clone();
    tokio::spawn(
        async move {
            let t0 = Instant::now();
            let end = ndjson(svc.clone(), body, tx).await;
            if let Some((caller, w, request_id)) = ui {
                audit_ui_stream(&svc, &caller, &w, request_id, end, t0.elapsed().as_secs_f64() * 1e3);
            }
        }
        .instrument(tracing::Span::current()),
    );
    let lines = tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(|j| Ok::<_, std::convert::Infallible>(Bytes::from(format!("{j}\n"))));
    Ok(([(header::CONTENT_TYPE, "application/x-ndjson")], axum::body::Body::from_stream(lines)).into_response())
}

/// How a stream ended ([`ndjson`]): rows written, lines done, the tables
/// its operations named, and the error it stopped at, if any.
#[derive(Default)]
struct StreamEnd {
    written: u64,
    lines: u64,
    tables: std::collections::BTreeSet<String>,
    error: Option<ApiError>,
}

/// The audit entry of a stream the command center sent, once it ended:
/// the tables it wrote, its rows and lines, and how it ended.
fn audit_ui_stream(svc: &Service, caller: &keys::Caller, w: &Principal, request_id: Option<String>, end: StreamEnd, ms: f64) {
    let tables: Vec<String> = end.tables.into_iter().collect();
    let list = tables.join(", ");
    let stopped = end.error.as_ref().map_or(String::new(), |e| format!("; it stopped: {}", e.message));
    svc.audit_add(keys::Entry {
        agent: None,
        on_behalf_of: None,
        change_request: None,
        ts_ms: telemetry::now_ms(),
        who: keys::audit_name(w),
        key: w.key.clone(),
        scope: Some(w.scope.name()),
        method: "POST".into(),
        route: "/v1/stream".into(),
        status: end.error.as_ref().map_or(200, |e| e.status),
        ms,
        request_id,
        detail: Some(format!("streamed {} rows in {} lines to {}{stopped}", end.written, end.lines, if list.is_empty() { "no table" } else { list.as_str() })),
        kind: keys::CHANGE.into(),
        addr: caller.addr.clone(),
        client: caller.named(),
        reason: caller.reason.clone(),
        object: match tables.len() {
            0 => None,
            1 => Some(format!("table {list}")),
            _ => Some(format!("tables {list}")),
        },
        before: None,
        after: Some(json!({"rows": end.written, "lines": end.lines, "tables": tables, "error": end.error.map(|e| e.message)})),
        actor: "person".into(),
    });
}

/// Lines a stream reads ahead of its writes ([`read_lines`]).
const STREAM_LINES: usize = 4096;

/// Writes a stream's lines ([`stream`]), answering on `tx`. The body is
/// read in a task of its own, ahead of the writes, so each batch takes the
/// lines that arrived while the one before it committed: one line at a
/// time while the client trickles, up to [`STREAM_OPS`] operations when
/// it sends faster than one commit takes. The client may send
/// acknowledged batches' successors before the answers arrive: answers
/// wait in memory, so that a client that reads them only once it has
/// sent everything does not stall the stream.
async fn ndjson(svc: Arc<Service>, body: axum::body::Body, tx: tokio::sync::mpsc::UnboundedSender<Json>) -> StreamEnd {
    let (ahead, mut lines) = tokio::sync::mpsc::channel(STREAM_LINES);
    tokio::spawn(read_lines(body, svc.max_body, ahead).in_current_span());
    let mut s = Stream::default();
    let mut batch: Vec<Json> = Vec::new();
    // Lines read, and lines committed.
    let (mut read, mut done) = (0u64, 0u64);
    let mut tables = std::collections::BTreeSet::new();
    let first_table = svc.default_table().map_or_else(|_| svc.first_table().table.clone(), |t| t.table.clone());
    let end = |s: &Stream, done: u64, tables: std::collections::BTreeSet<String>, error: Option<ApiError>| StreamEnd { written: s.written, lines: done, tables, error };
    while let Some(first) = lines.recv().await {
        let mut next = Some(first);
        while let Some(line) = next.take() {
            let ops = match line {
                Ok(ops) => s.admit(&ops).map(|_| ops).map_err(|e| ApiError { message: format!("line {}: {}", read + 1, e.message), ..e }),
                Err(e) => Err(e),
            };
            let ops = match ops {
                Ok(ops) => ops,
                Err(e) => {
                    // The lines before it are good.
                    if let Err(f) = flush(&svc, &mut s, &mut batch, read, &mut done, &tx).await {
                        return end(&s, done, tables, Some(f));
                    }
                    let _ = tx.send(json!({"error": e.message, "status": e.status, "lines": done}));
                    return end(&s, done, tables, Some(e));
                }
            };
            read += 1;
            for op in &ops {
                if !is_mark(op) {
                    tables.insert(op["table"].as_str().map_or_else(|| first_table.clone(), String::from));
                }
            }
            batch.extend(ops);
            if batch.len() < STREAM_OPS {
                next = lines.try_recv().ok();
            }
        }
        if let Err(f) = flush(&svc, &mut s, &mut batch, read, &mut done, &tx).await {
            return end(&s, done, tables, Some(f));
        }
    }
    if let Err(f) = flush(&svc, &mut s, &mut batch, read, &mut done, &tx).await {
        return end(&s, done, tables, Some(f));
    }
    let _ = tx.send(json!({"done": true, "batches": s.batches, "lines": done, "written": s.written, "skipped": s.skipped}));
    end(&s, done, tables, None)
}

/// Reads a stream's body into its lines' operations ([`line_ops`]) for
/// [`ndjson`] to write. Once [`STREAM_LINES`] lines wait it stops reading,
/// so that flow control slows a producer faster than the writes. A line
/// that is not operations is the last it sends.
async fn read_lines(body: axum::body::Body, max: usize, lines: tokio::sync::mpsc::Sender<Result<Vec<Json>, ApiError>>) {
    let at_line = |n: u64| move |e: ApiError| ApiError { message: format!("line {n}: {}", e.message), ..e };
    let mut chunks = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut n = 0u64;
    while let Some(next) = chunks.next().await {
        let bytes = match next {
            Ok(b) => b,
            Err(e) => {
                let _ = lines.send(Err(ApiError::bad(format!("reading the body: {e}")))).await;
                return;
            }
        };
        buf.extend_from_slice(&bytes);
        let mut at = 0;
        while let Some(nl) = buf[at..].iter().position(|&b| b == b'\n') {
            n += 1;
            let ops = line_ops(&buf[at..at + nl]).map_err(at_line(n));
            at += nl + 1;
            let bad = ops.is_err();
            if lines.send(ops).await.is_err() || bad {
                return;
            }
        }
        buf.drain(..at);
        if buf.len() > max {
            let _ = lines.send(Err(ApiError { code: None, status: 413, message: format!("line {}: longer than the server takes ({max} bytes)", n + 1) })).await;
            return;
        }
    }
    // A last line without its newline.
    if !buf.is_empty() {
        let _ = lines.send(line_ops(&buf).map_err(at_line(n + 1))).await;
    }
}

/// Writes the stream's batch, if any, and answers it; its error when it
/// failed, which the answer says.
async fn flush(svc: &Arc<Service>, s: &mut Stream, batch: &mut Vec<Json>, read: u64, done: &mut u64, tx: &tokio::sync::mpsc::UnboundedSender<Json>) -> Result<(), ApiError> {
    if batch.is_empty() {
        *done = read;
        return Ok(());
    }
    let ops = std::mem::take(batch);
    let n = ops.len();
    match svc.stream_write(s, ops).await {
        Ok(w) => {
            *done = read;
            let mut j = json!({"batch": s.batches, "ops": n, "written": w.rows, "skipped": w.skipped, "lines": read});
            if let Some(q) = w.seq_no {
                j["seq_no"] = json!(q);
            }
            if !w.seq_nos.is_empty() {
                j["seq_nos"] = json!(w.seq_nos);
            }
            let offsets = s.offsets();
            if !offsets.is_empty() {
                j["offsets"] = Json::Object(offsets.into_iter().map(|(n, o)| (n, json!(o))).collect());
            }
            let _ = tx.send(j);
            Ok(())
        }
        Err(e) => {
            let _ = tx.send(json!({"error": e.message, "status": e.status, "lines": *done}));
            Err(e)
        }
    }
}

/// A line of a stream: an operation, `{"ops": [...]}`, or blank.
fn line_ops(line: &[u8]) -> Result<Vec<Json>, ApiError> {
    let text = std::str::from_utf8(line).map_err(|_| ApiError::bad("not UTF-8 text"))?.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut j: Json = serde_json::from_str(text).map_err(|e| ApiError::bad(format!("not JSON: {e}")))?;
    match j.get_mut("ops").map(Json::take) {
        Some(Json::Array(mut ops)) => {
            if let Some(src) = j.get("source").filter(|s| !s.is_null()) {
                for op in ops.iter_mut().filter(|op| op.is_object() && !has_source(op)) {
                    op["source"] = src.clone();
                }
            }
            Ok(ops)
        }
        Some(_) => Err(ApiError::bad("\"ops\" is an array of operations")),
        None if j.is_object() => Ok(vec![j]),
        None => Err(ApiError::bad("a line is an operation (a JSON object), or one event's operations as {\"ops\": [...]}")),
    }
}

/// Every stream source written from, with the offset of the last event
/// each wrote ([`Service::sources`]): on a coordinator the least over its
/// shards; `?shards=true` also lists each shard's own (`shards`).
async fn sources(State(app): State<App>, Query(q): Query<std::collections::HashMap<String, String>>) -> Res {
    if flag(&q, "shards", false)? {
        let by_shard = app.svc.sources_by_shard().await?;
        let least = match app.svc.coordinator() {
            Ok(_) => app.svc.sources().await?,
            Err(_) => by_shard.first().cloned().unwrap_or_default(),
        };
        let shards: Vec<Json> = by_shard.into_iter().map(|s| Json::Array(s.into_iter().map(source_json).collect())).collect();
        return Ok(AxJson(json!({"sources": least.into_iter().map(source_json).collect::<Vec<_>>(), "shards": shards})));
    }
    let sources: Vec<Json> = app.svc.sources().await?.into_iter().map(source_json).collect();
    Ok(AxJson(json!({"sources": sources})))
}

/// `GET /v1/admin/follow`: the clusters this one follows or followed
/// ([`crate::follow`]): whether it follows each now, how far its follow
/// process applied each source shard's feed, and what that process last
/// said of itself.
async fn follow_view(State(app): State<App>) -> Res {
    Ok(AxJson(app.svc.follow_view().await?))
}

/// `POST /v1/admin/follow/status`: a follow process says how it stands
/// (the cluster's token only).
async fn follow_status(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    root_only(&who(w), "a follow process's status")?;
    let name = b["name"].as_str().filter(|n| crate::service::follow_name_ok(n)).ok_or_else(|| ApiError::bad("the status names its cluster: {\"name\": ...}"))?.to_string();
    app.svc.note_follow_status(&name, b);
    Ok(AxJson(json!({"noted": name})))
}

/// `POST /v1/admin/follow/stop {"name"}`: stops following cluster `name`
/// (an admin's call): from then on this cluster takes its clients'
/// writes, and refuses its follow process's. When the operator runs that
/// process (`spec.follow`), its spec goes too ([`follow_spec`]).
async fn follow_stop(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let name = b["name"].as_str().ok_or_else(|| ApiError::bad("name the cluster to stop following: {\"name\": ...}"))?;
    let mut r = app.svc.stop_following(name).await?;
    keys::note(format!("stopped following {name}"));
    if let Some(s) = follow_spec(&app, name, Json::Null).await {
        r["spec"] = s;
    }
    Ok(AxJson(r))
}

/// `POST /v1/admin/follow/pause {"name", "paused"?: true}`: the operator's
/// follow process of `name` stops for now (`spec.follow.paused`), or
/// starts again from where it was; the cluster still refuses its
/// clients' writes meanwhile. Only where the operator runs it.
async fn follow_pause(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let name = b["name"].as_str().ok_or_else(|| ApiError::bad("name the cluster this one follows: {\"name\": ..., \"paused\"?: true}"))?;
    let paused = b.get("paused").map(|p| p.as_bool().ok_or_else(|| ApiError::bad("paused is true or false"))).transpose()?.unwrap_or(true);
    match follow_spec(&app, name, json!({"paused": paused})).await {
        None => Err(ApiError { code: None, status: 409, message: format!("the follow process of {name} is not this cluster's operator's (no spec.follow names it): stop or start it where it runs") }),
        Some(s) if s.get("error").is_some() => Err(ApiError { code: None, status: 502, message: format!("the Kubernetes API: {}", s["error"].as_str().unwrap_or_default()) }),
        Some(s) => {
            keys::note(format!("{} following {name}", if paused { "paused" } else { "resumed" }));
            Ok(AxJson(json!({"name": name, "paused": paused, "spec": s})))
        }
    }
}

/// When the operator runs this cluster's follow process of `name`
/// (`spec.follow` names it): merges `patch` into `spec.follow` (null:
/// none), and answers `{"changed": true}` or the error. None when it does
/// not.
async fn follow_spec(app: &App, name: &str, patch: Json) -> Option<Json> {
    let l = app.svc.kube.as_ref()?;
    let now = match l.get().await {
        Ok(j) => j,
        Err(e) => return Some(json!({"error": e})),
    };
    if now["spec"]["follow"]["name"] != name {
        return None;
    }
    Some(match l.change(&json!({"follow": patch}), &[]).await {
        Ok(_) => json!({"changed": true}),
        Err(e) => {
            tracing::warn!(error = %e, "could not change spec.follow");
            json!({"error": e})
        }
    })
}

/// `POST /v1/admin/follow/promote {"name", "unplanned"?}`: promotes this
/// cluster, which follows cluster `name`: it stops following and takes its
/// own clients' writes ([`crate::service::Service::promote_follower`]).
/// Refused (409) unless `name` held its writes on every shard as far as
/// this cluster applied, or with `"unplanned": true`, which loses the
/// writes it did not apply. An admin's call, as stopping is; where the
/// operator runs the follow process, its spec goes ([`follow_spec`]).
async fn follow_promote(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let name = b["name"].as_str().ok_or_else(|| ApiError::bad("name the cluster this one follows: {\"name\": ..., \"unplanned\"?: false}"))?;
    let unplanned = b.get("unplanned").map(|u| u.as_bool().ok_or_else(|| ApiError::bad("unplanned is true or false"))).transpose()?.unwrap_or(false);
    let mut r = app.svc.promote_follower(name, unplanned).await?;
    if let Some(s) = follow_spec(&app, name, Json::Null).await {
        r["spec"] = s;
    }
    keys::note(match unplanned {
        true => format!("promoted from {name}, unplanned: {} transactions lost", r["lost"]),
        false => format!("promoted from {name}"),
    });
    Ok(AxJson(r))
}

/// `POST /v1/admin/hold-writes {"hold"?: true}`: holds this cluster's
/// clients' writes on every shard (503 to each from then on), as a
/// switch-over to a cluster that follows it begins; `{"hold": false}`
/// ends the hold. `GET`: whether it holds them.
async fn hold_writes(State(app): State<App>, b: Option<AxJson<Json>>) -> Res {
    let on = match b.as_ref().and_then(|AxJson(b)| b.get("hold")) {
        None => true,
        Some(h) => h.as_bool().ok_or_else(|| ApiError::bad("hold is true or false"))?,
    };
    let r = app.svc.hold_writes(on).await?;
    keys::note(if on { "held the cluster's writes" } else { "ended the hold of the cluster's writes" }.to_string());
    Ok(AxJson(r))
}

async fn hold_view(State(app): State<App>) -> Res {
    Ok(AxJson(app.svc.hold_view().await?))
}

/// `GET /v1/feed/promoted`: the clusters this one was promoted from, with
/// its shards' numbers then, by which the old source follows it back
/// ([`crate::service::Service::promotions`]).
async fn feed_promoted(State(app): State<App>) -> Res {
    Ok(AxJson(app.svc.promotions().await?))
}

fn root_only(w: &Principal, what: &str) -> Result<(), ApiError> {
    match w.root {
        true => Ok(()),
        false => Err(ApiError { code: None, status: 403, message: format!("{what} takes the cluster's token, not a key") }),
    }
}

/// One source's offset, null for one not written from (start from the
/// beginning).
async fn source(State(app): State<App>, axum::extract::Path(name): axum::extract::Path<String>) -> Res {
    let found = app.svc.sources().await?.into_iter().find(|s| s.name == name);
    Ok(AxJson(found.map_or_else(|| json!({"name": name, "offset": null, "updated": null}), source_json)))
}

fn source_json(s: chloedb::db::Source) -> Json {
    json!({"name": s.name, "offset": s.offset, "updated": s.updated.map(chloedb::timestamp::format)})
}

/// Whether a statement would run on this deployment, without running it
/// ([`crate::sql_check`]): `{runs, deployment, kind, error?, notes}`.
async fn sql_check(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    let sql = body_str(&b, "sql")?;
    let params = app.svc.params(None, b.get("params")).await?;
    let budget = app.svc.budget_for(Some(&who(w)), body_budget(&b)?);
    Ok(AxJson(app.svc.sql_check(sql, &params, body_u64(&b, "timeout_ms")?, budget).await?))
}

/// A schema validated, in both JSON forms and as the `CREATE TABLE`
/// statement that makes it.
async fn schema_check(AxJson(b): AxJson<Json>) -> Res {
    let s = chloedb::schema::Schema::from_json_value(&b)?;
    Ok(AxJson(json!({"schema": s, "short": s.to_short_json(), "sql": chloedb::query::ddl::create_sql(&s)})))
}

/// The vector of a text, or of an image or sound file (`"image"` or
/// `"audio"`, base64), as `{"embed": ...}` gives a query's parameter: for
/// a plan, whose vectors are values.
/// The embedding models a schema can name, local and hosted, and whether
/// each would open on this server ([`chloedb::embed::catalog`]): fetched,
/// its key set, its feature built. Nothing is loaded or sent.
/// The models a field may name ([`chloedb::embed::catalog`]), each with
/// how it makes its vectors ([`chloedb::embed::describe`]), the fields
/// that name it, and a fetch or conversion asked for here.
async fn models(State(app): State<App>, w: Option<axum::Extension<Principal>>) -> Res {
    let who = who(w);
    let dir = app.svc.models_dir();
    let d = dir.clone();
    let list = tokio::task::spawn_blocking(move || chloedb::embed::catalog(&d).into_iter().map(|m| { let x = chloedb::embed::describe(&m.name, &d); (m, x) }).collect::<Vec<_>>()).await.map_err(ApiError::internal)?;
    // A key limited to tables sees the fields of the tables it reads alone.
    let fields: Vec<crate::service::FieldModel> = app.svc.models().iter().filter(|f| who.table_scope(&f.table).is_some_and(|s| s >= Scope::Read)).cloned().collect();
    let mut jobs = app.svc.model_jobs();
    let mut models: Vec<Json> = list
        .iter()
        .map(|(m, d)| {
            let used: Vec<Json> = fields.iter().filter(|f| f.spec.model.eq_ignore_ascii_case(&m.name)).map(|f| json!({"table": f.table, "field": f.field})).collect();
            json!({
                "name": m.name, "dim": m.dim, "hosted": m.hosted, "what": m.what, "ready": m.ready.is_ok(), "error": m.ready.as_ref().err(),
                "technique": d.technique, "precision": d.precision, "bytes": d.bytes, "made_of": d.made_of,
                // Fetched by the server for a field that names it.
                "known": chloedb::embed::KNOWN_MODELS.iter().any(|k| k.0.eq_ignore_ascii_case(&m.name)),
                "fields": used, "job": jobs.remove(&m.name.to_lowercase()),
            })
        })
        .collect();
    // A conversion that failed before its directory held a model.
    for (name, job) in jobs {
        models.push(json!({"name": name, "dim": null, "hosted": false, "what": "", "ready": false, "error": job.error, "technique": "static", "fields": [], "job": job}));
    }
    // Names a field gave that the catalog does not list (`hash:16`).
    for f in fields.iter() {
        if !models.iter().any(|m| m["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&f.spec.model))) {
            let d = chloedb::embed::describe(&f.spec.model, &dir);
            let used: Vec<Json> = fields.iter().filter(|g| g.spec.model == f.spec.model).map(|g| json!({"table": g.table, "field": g.field})).collect();
            models.push(json!({"name": f.spec.model, "dim": chloedb::embed::known_dim(&f.spec.model), "hosted": d.technique == "hosted", "what": "", "ready": f.model.is_ok(), "error": f.model.as_ref().err(), "technique": d.technique, "precision": d.precision, "bytes": d.bytes, "known": false, "fields": used, "job": null}));
        }
    }
    Ok(AxJson(json!({"dir": dir.display().to_string(), "fetches": app.svc.fetch_models, "models": models})))
}

/// `{"model": name}`: fetches a model the server knows into its models
/// directory, in the background ([`Service::start_model_job`]).
async fn fetch_model(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let name = body_str(&b, "model")?;
    let made = app.svc.start_model_job(name, None)?;
    Ok(AxJson(json!({"model": made, "state": "running"})))
}

/// `{"model": name, "to": "I8" | "F16"}`: writes a static model's table
/// again in fewer bits, as model `<name>-int8` (or `-f16`), in the
/// background ([`Service::start_model_job`]).
async fn convert_model(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let name = body_str(&b, "model")?;
    let to = b.get("to").and_then(Json::as_str).unwrap_or("I8");
    let made = app.svc.start_model_job(name, Some(to))?;
    Ok(AxJson(json!({"model": made, "from": name, "state": "running"})))
}

async fn embed(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    let who = who(w);
    let file = ["image", "audio"].into_iter().find_map(|k| b.get(k).map(|v| (k, v)));
    let what = match file {
        Some((k, v)) => json!({ k: v.as_str().ok_or_else(|| ApiError::bad(format!("{k} is a file in base64")))? }),
        None => json!(body_str(&b, "text")?),
    };
    let name = |k: &str| match b.get(k) {
        None | Some(Json::Null) => Ok(None),
        Some(f) => f.as_str().map(Some).ok_or_else(|| ApiError::bad(format!("{k} is a name"))),
    };
    // The table whose model embeds it, which the caller reads: the first
    // such it may, when none is named ([`Service::embed_target`]).
    let (table, field) = app.svc.embed_target(name("table")?, name("field")?, &[], &who)?;
    let p = app.svc.params_for(Some(&json!({ "v": { "embed": what, "field": field, "table": table } })), None, &who).await?;
    match p.0.get("v") {
        Some(chloedb::value::Value::Vector(v)) => Ok(AxJson(json!({ "table": table, "field": field, "vector": &v[..] }))),
        // A multi-vector field's model: a vector a token of the text.
        Some(chloedb::value::Value::List(vs)) => {
            let vectors: Vec<&[f32]> = vs.iter().filter_map(|v| if let chloedb::value::Value::Vector(v) = v { Some(&v[..]) } else { None }).collect();
            Ok(AxJson(json!({ "table": table, "field": field, "vectors": vectors })))
        }
        _ => Err(ApiError::internal("the text has no vector")),
    }
}

/// The file a MEDIA value names, by its SHA-256, for showing it: only a
/// file some row names, read from where the row says and checked against
/// the hash ([`Service::media_file`]). Its bytes never change, so clients
/// may keep it. The bytes are the writer's and so is the type a row
/// gives them: they are served so that a browser never runs them on the
/// server's origin ([`media_headers`]).
async fn media_file(State(app): State<App>, axum::extract::Path(sha256): axum::extract::Path<String>) -> Result<Response, ApiError> {
    let (mime, bytes) = app.svc.media_file(&sha256).await?;
    Ok((media_headers(&mime, &sha256), bytes).into_response())
}

/// The headers of a stored file: its type only when it is one a browser
/// shows without running anything (a raster image, a sound, a video),
/// else `application/octet-stream`; any other file, a PDF too, as a
/// download (`Content-Disposition: attachment`); never sniffed for
/// another type (`nosniff`); and, should it be opened as a page, in a
/// sandbox with nothing it may load or run.
pub fn media_headers(mime: &str, sha256: &str) -> HeaderMap {
    const SHOWN: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp", "image/bmp", "image/avif"];
    let essence = mime.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let token = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b));
    let well_formed = essence.split_once('/').is_some_and(|(t, s)| token(t) && token(s));
    let shown = well_formed && (SHOWN.contains(&essence.as_str()) || essence.starts_with("audio/") || essence.starts_with("video/"));
    let kept = well_formed && (shown || essence == "application/pdf");
    let mut h = HeaderMap::new();
    let ty = if kept { essence } else { "application/octet-stream".to_string() };
    h.insert(header::CONTENT_TYPE, header::HeaderValue::from_str(&ty).unwrap_or(header::HeaderValue::from_static("application/octet-stream")));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, header::HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_SECURITY_POLICY, header::HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"));
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("private, max-age=31536000, immutable"));
    if !shown {
        let name = if chloedb::media::is_sha256(sha256) { sha256 } else { "file" };
        h.insert(header::CONTENT_DISPOSITION, header::HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")).unwrap_or(header::HeaderValue::from_static("attachment")));
    }
    h
}

async fn explain(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    let sql = body_str(&b, "sql")?;
    let who = who(w);
    reads_ok(&app, &who, sql).await?;
    let params = app.svc.params_for(b.get("params"), Some(sql), &who).await?;
    // A query that asks a language model about its rows: the plan of the
    // rows it asks about, and nothing asked.
    let first = chloedb::query::llm::split_sql(sql, &app.svc.functions(), &params)?.map(|s| s.first_sql()).transpose()?;
    let sql = first.as_deref().unwrap_or(sql);
    let catalog = app.svc.catalog();
    let tables: Vec<&chloedb::schema::Schema> = catalog.iter().map(|t| &**t).collect();
    // A compound query is operators over single-table queries; what each
    // of those ran is in "plan".
    let profiles = |t: &str, n: &str| app.svc.rank_profile(t, n);
    // The plan names the files of its ranking models. It is made as the
    // query's would be: off the request's thread when it may take long,
    // stopped at the deadline, within the statement's budget (X-B12).
    let budget = body_budget(&b)?;
    let timeout_ms = body_u64(&b, "timeout_ms")?;
    let plan = app.svc.open_ranking_models(std::sync::Arc::new(app.svc.plan_for_explain(sql, &params, timeout_ms, app.svc.budget_for(Some(&who), budget)).await?), false)?;
    let logical = match plan.root.uses_operators() {
        true => "a compound query: operators over single-table queries (\"tree\"), whose runs are in \"plan\"".to_string(),
        false => {
            let schema = tables.iter().find(|t| t.table.eq_ignore_ascii_case(plan.table())).ok_or_else(|| ApiError::bad(format!("query error: no such table: {}", plan.table())))?;
            // A query by a rank profile binds as written out.
            format!("{:#?}", chloedb::query::bind(&chloedb::query::profile::written_out(sql, &profiles)?, schema, &params)?)
        }
    };
    let (ir, tree) = (plan.to_json()?, json!(plan.explain(&tables)));
    // `hits`: the top rows with the values they were ranked by, for a
    // query that ranks them (another has no values to show).
    // A query the hits cannot be shown for (one that orders groups, or by
    // a column it computes) is explained without them, and says why.
    let mut no_hits = None;
    let hits = match body_u64(&b, "hits")? {
        0 => None,
        _ if !chloedb::query::sql::ranks(sql) => None,
        n => {
            let asked = match chloedb::query::sql::with_features(sql) {
                Ok(sql) => app.svc.query(QueryReq { sql, params: params.clone(), timeout_ms, who: Some(who.clone()), budget, ..Default::default() }).await,
                Err(e) => Err(ApiError::from(e)),
            };
            match asked {
                Ok(h) => {
                    let j = js::result(&h.rs, h.took_ms);
                    let names = j["columns"].as_array().cloned().unwrap_or_default();
                    let rows = j["rows"].as_array().cloned().unwrap_or_default();
                    Some(rows.iter().take(n as usize).map(|r| names.iter().zip(r.as_array().into_iter().flatten()).map(|(c, v)| (c.as_str().unwrap_or("").to_string(), v.clone())).collect::<serde_json::Map<_, _>>()).collect::<Vec<_>>())
                }
                Err(e) => {
                    no_hits = Some(e.message);
                    None
                }
            }
        }
    };
    let mut a = app.svc.query(QueryReq { sql: sql.into(), params, timeout_ms, profile: b.get("profile").and_then(|v| v.as_bool()) == Some(true), who: Some(who.clone()), budget, ..Default::default() }).await?;
    hide_hosts(&who, &mut a.rs.stats.steps);
    let mut out = json!({
        "kind": a.kind,
        "logical": logical,
        "ir": ir,
        "tree": tree,
        "plan": a.rs.stats.plan,
        "filter_docs": a.rs.stats.filter_docs,
        "scored": a.rs.stats.scored,
        "steps": a.rs.stats.steps,
        "rows": a.rs.rows.len(),
        "took_ms": a.took_ms,
    });
    if let Some(h) = hits {
        out["hits"] = json!(h);
    }
    if let Some(why) = no_hits {
        out["hits_skipped"] = json!(why);
    }
    Ok(AxJson(out))
}

async fn synonyms(State(app): State<App>) -> AxJson<Json> {
    let s = app.svc.synonyms();
    let tables: Vec<Json> = app
        .svc
        .table_synonyms()
        .into_iter()
        .map(|(table, text, version)| {
            let rules = chloedb::text::Synonyms::parse(&text).map_or(0, |s| s.rules());
            json!({"table": table, "text": text, "rules": rules, "version": version})
        })
        .collect();
    AxJson(json!({
        "text": s.as_ref().map_or("", |s| s.text()),
        "rules": s.as_ref().map_or(0, |s| s.rules()),
        "file": app.svc.synonyms_file().map(|p| p.display().to_string()),
        "tables": tables,
    }))
}

/// `{table, field, vector? | log?, min_cos?, per_word?, max_words?,
/// min_df?, max_rows?, window?, min_support?}`: synonyms of `table.field`
/// mined from the vectors of its words (by the model of vector field
/// `vector`) or from the searches of table `log` (`context`, `ts`,
/// `text`), as proposals and as the text of a list
/// ([`chloedb::query::mine`]).
async fn mine_synonyms(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    use chloedb::query::mine::{Options, Request, Source};
    let (table, field) = (body_str(&b, "table")?.to_string(), body_str(&b, "field")?.to_string());
    let source = match (b.get("vector").and_then(Json::as_str), b.get("log").and_then(Json::as_str)) {
        (Some(v), None) => Source::Vectors { vector: v.to_string() },
        (None, Some(l)) => Source::Log { log: l.to_string() },
        _ => return Err(ApiError::bad("name one source: vector (a vector field of the table, whose model embeds the words) or log (a table of searches: context, ts, text)")),
    };
    let d = Options::default();
    let n = |k: &str, d: usize, max: usize| b.get(k).and_then(Json::as_u64).map_or(d, |v| (v as usize).clamp(1, max));
    let options = Options {
        max_rows: n("max_rows", d.max_rows, chloedb::query::mine::MAX_ROWS),
        max_words: n("max_words", d.max_words, chloedb::query::mine::MAX_WORDS),
        min_df: n("min_df", d.min_df, 1_000_000),
        min_cos: b.get("min_cos").and_then(Json::as_f64).map_or(d.min_cos, |c| c.clamp(chloedb::query::mine::MIN_COS as f64, 1.0) as f32),
        per_word: n("per_word", d.per_word, chloedb::query::mine::MAX_PER_WORD),
        window: b.get("window").and_then(Json::as_i64).map_or(d.window, |w| w.max(0)),
        min_support: n("min_support", d.min_support, 1_000_000),
    };
    let m = app.svc.mine_synonyms(Request { table, field, source, options }).await?;
    Ok(AxJson(serde_json::to_value(m).map_err(ApiError::internal)?))
}

async fn put_synonyms(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let rules = app.svc.put_synonyms(body_str(&b, "text")?)?;
    Ok(AxJson(json!({"rules": rules, "file": app.svc.synonyms_file().map(|p| p.display().to_string())})))
}

/// `{"field": "sid", "prefixes": ["12", "12.7"], "where": "tag = :t",
/// "params": {...}, "table": ..., "min_seq_no": ..., "timeout_ms": ...}`:
/// the codes that can follow each prefix among the rows `where` matches
/// (all rows without it), with how many rows have each
/// ([`chloedb::db::Db::sid_next`]). `table` defaults to the first.
async fn sid_next(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let t0 = Instant::now();
    let table = match b.get("table") {
        None | Some(Json::Null) => app.svc.schema.table.clone(),
        Some(_) => body_str(&b, "table")?.to_string(),
    };
    let field = body_str(&b, "field")?;
    let prefixes: Vec<String> = match b.get("prefixes") {
        Some(Json::Array(ps)) => ps.iter().map(|p| p.as_str().map(String::from).ok_or_else(|| ApiError::bad("prefixes are strings, such as \"12.7\", or \"\" for the first numbers"))).collect::<Result<_, _>>()?,
        _ => return Err(ApiError::bad("the body needs \"prefixes\", a list of codes such as [\"12\", \"12.7\"] (\"\" for the first numbers)")),
    };
    let filter = match b.get("where") {
        None | Some(Json::Null) => String::new(),
        Some(_) => format!(" WHERE {}", body_str(&b, "where")?),
    };
    let sql = format!("SELECT COUNT(*) FROM {}{filter}", chloedb::query::sql::ident(&table));
    let params = app.svc.params(None, b.get("params")).await?;
    let next = app.svc.sid_next(&sql, None, &params, field, &prefixes, &after(&b)?, body_u64(&b, "timeout_ms")?).await?;
    let next: Vec<Json> = prefixes
        .iter()
        .zip(next)
        .map(|(p, cs)| json!({"prefix": p, "next": cs.into_iter().map(|(code, count)| json!({"code": code, "count": count})).collect::<Vec<_>>()}))
        .collect();
    Ok(AxJson(json!({"next": next, "took_ms": t0.elapsed().as_secs_f64() * 1e3})))
}

async fn term_stats(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let params = app.svc.params(None, b.get("params")).await?;
    let (stats, snapshot, version) = app.svc.term_stats(body_str(&b, "sql")?, None, &params, &after(&b)?, body_u64(&b, "timeout_ms")?).await?;
    Ok(AxJson(json!({"stats": stats, "snapshot": snapshot, "version": version})))
}

/// `{table, field, text}`: "did you mean", the text with the words that
/// look like typos of the field's terms replaced ([`chloedb::db::Db::correct`]);
/// a coordinator's by its shards' counts ([`crate::service::Service::correct`]).
async fn correct(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let (table, field, text) = (body_str(&b, "table")?.to_string(), body_str(&b, "field")?.to_string(), body_str(&b, "text")?.to_string());
    let c = app.svc.correct(&table, &field, &text, body_u64(&b, "timeout_ms")?).await?;
    Ok(AxJson(serde_json::to_value(c).map_err(ApiError::internal)?))
}

/// `{table, field, text, mode?, fields?, weight?}`: what the words say for
/// the table's keyword and numeric fields, as `bm25(.., understand => ..)`
/// takes them.
async fn understand(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let (table, field, text) = (body_str(&b, "table")?, body_str(&b, "field")?, body_str(&b, "text")?);
    let mut opts = chloedb::query::understand::Options::default();
    if let Some(m) = b.get("mode").and_then(Json::as_str) {
        opts.mode = chloedb::query::understand::Mode::of(m)?;
    }
    match b.get("fields") {
        Some(Json::String(f)) => opts.fields = Some(f.split(',').map(|f| f.trim().to_string()).filter(|f| !f.is_empty()).collect()),
        Some(Json::Array(fs)) => opts.fields = Some(fs.iter().filter_map(|f| f.as_str().map(str::to_string)).collect()),
        _ => {}
    }
    if let Some(w) = b.get("weight").and_then(Json::as_f64) {
        opts.weight = w;
    }
    let u = app.svc.understand(table, field, text, &opts).await?;
    let mut v = serde_json::to_value(&u).map_err(ApiError::internal)?;
    v["filters"] = json!(u.filters());
    v["boosts"] = json!(u.boosts());
    v["line"] = json!(u.line());
    Ok(AxJson(v))
}

/// `{name, text, k?}`: the classes of a text by a classifier `CREATE
/// CLASSIFIER` made, its `k` best (3).
async fn classify(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let (name, text) = (body_str(&b, "name")?, body_str(&b, "text")?);
    let k = b.get("k").and_then(Json::as_u64).unwrap_or(3).clamp(1, 100) as usize;
    let c = app.svc.classify(name, text, k).await?;
    Ok(AxJson(serde_json::to_value(c).map_err(ApiError::internal)?))
}

/// The classifiers `CREATE CLASSIFIER` made, without their weights: what
/// each was trained on and what it measured.
async fn classifiers(State(app): State<App>) -> Res {
    app.svc.learn_functions().await;
    let set = app.svc.functions();
    let mut out: Vec<Json> = Vec::new();
    for f in set.currents() {
        let Ok(Some(c)) = chloedb::query::classify::of_function(f) else { continue };
        out.push(json!({
            "name": f.def.name, "version": f.version, "method": c.method, "table": c.table, "field": c.field, "labels_from": c.labels_from,
            "model": c.model, "dim": c.dim, "labels": c.labels.len(), "label_names": c.labels.iter().take(50).collect::<Vec<_>>(),
            "min_score": c.min_score, "trained_rows": c.trained_rows, "holdout_rows": c.holdout_rows, "holdout_accuracy": c.holdout_accuracy,
            "created": f.created.map(chloedb::timestamp::format), "created_by": f.created_by,
        }));
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(AxJson(json!({"classifiers": out})))
}

/// `{model, text}`: what a built-in content model (`builtin:lang`,
/// `builtin:keyphrases`, `builtin:minhash`) makes of a text, as a derived
/// KEYWORD field of it would hold: before the field exists, or for rows
/// written before it.
/// `{model, text, table?, field?, many?}`: what a content model makes of a
/// text, as a derived KEYWORD field of it would hold it. A built-in model
/// needs nothing else; a dictionary (`dict:<table>.<field>`) reads the
/// values its table holds now, the text analysed as `table`'s `field` is
/// (English without them); a classifier (`classifier:<name>`) gives its
/// best label, or with `many` the labels a `KEYWORD[]` field would hold.
async fn derive_preview(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let (model, text) = (body_str(&b, "model")?, body_str(&b, "text")?);
    let catalog = app.svc.catalog();
    let schema_of = |t: &str| catalog.iter().find(|s| s.table.eq_ignore_ascii_case(t)).cloned().ok_or_else(|| ApiError::bad(format!("no table {t}")));
    if model.starts_with(chloedb::content::DICT) {
        let analyzer = match (b.get("table").and_then(Json::as_str), b.get("field").and_then(Json::as_str)) {
            (Some(t), Some(f)) => schema_of(t)?.field(f).ok_or_else(|| ApiError::bad(format!("{t} has no field {f}")))?.text_analyzer(),
            _ => chloedb::text::Analyzer::English,
        };
        let d = chloedb::content::DictEntities::new(model, analyzer).map_err(|e| ApiError::bad(e.to_string()))?;
        let dict = schema_of(&d.table)?;
        let f = dict.field(&d.field).filter(|f| f.ty == chloedb::schema::FieldType::Keyword).ok_or_else(|| ApiError::bad(format!("{}.{} is not a KEYWORD field", dict.table, d.field)))?;
        let key = (dict.table.clone(), f.name.clone(), analyzer);
        if let Some(g) = app.svc.gazetteers(std::slice::from_ref(&key), None).await?.get(&key) {
            d.set(g.clone());
        }
        return Ok(AxJson(json!({"model": model, "many": true, "values": d.of(text)})));
    }
    if let Some(name) = chloedb::content::classifier_name(model) {
        let many = b.get("many").and_then(Json::as_bool).unwrap_or(false);
        let c = app.svc.classify(name, text, 10).await?;
        let values: Vec<&str> = match many {
            false => c.label.as_deref().into_iter().collect(),
            true => c.top.iter().filter(|(_, p)| *p >= c.min_score && *p >= 0.1).map(|(l, _)| l.as_str()).collect(),
        };
        return Ok(AxJson(json!({"model": model, "many": many, "values": values, "top": c.top})));
    }
    let d = chloedb::content::open(model).map_err(|e| ApiError::bad(e.to_string()))?;
    let made = d.derive(&[chloedb::embed::Input::Text(text)])?;
    let values: Vec<&str> = made.first().map(|v| v.split(chloedb::content::SEP).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
    Ok(AxJson(json!({"model": model, "many": chloedb::content::makes_many(model), "values": values})))
}

async fn snapshot(State(app): State<App>) -> Res {
    let (id, txn) = app.svc.lease()?;
    Ok(AxJson(json!({"snapshot": id, "txn": txn, "ttl_s": app.svc.lease_ttl.as_secs()})))
}

/// `{"name": ..., "layout": "file" | "dir", "link_from": ...}`: writes a
/// checkpoint into the server's `--backup-dir`.
async fn admin_checkpoint(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let layout = match b.get("layout").and_then(|v| v.as_str()).unwrap_or("file") {
        "file" => Layout::SingleFile,
        "dir" | "directory" => Layout::Directory,
        other => return Err(ApiError::bad(format!("layout is \"file\" or \"dir\", not {other:?}"))),
    };
    let stats = app.svc.checkpoint(body_str(&b, "name")?, layout, b.get("link_from").and_then(|v| v.as_str())).await?;
    Ok(AxJson(json!({"checkpoint": stats})))
}

/// `{"rate_mib_s"?}`: reads every segment against its checksum now
/// ([`Service::scrub`]); unpaced unless asked.
async fn admin_scrub(State(app): State<App>, w: Option<axum::Extension<Principal>>, body: Option<AxJson<Json>>) -> Res {
    let rate = body.as_ref().and_then(|b| b.get("rate_mib_s")).and_then(Json::as_u64).unwrap_or(0) << 20;
    Ok(AxJson(app.svc.scrub(rate, &keys::audit_name(&who(w))).await?))
}

async fn admin_check(State(app): State<App>) -> Res {
    app.svc.check().await?;
    Ok(AxJson(json!({"ok": true})))
}

async fn admin_vacuum(State(app): State<App>) -> Res {
    Ok(AxJson(json!({"bytes_freed": app.svc.vacuum().await?})))
}

async fn admin_verify(State(app): State<App>, body: Option<AxJson<Json>>) -> Res {
    let timeout_ms = body.as_ref().and_then(|b| b.get("timeout_ms")).and_then(|v| v.as_u64()).unwrap_or(0);
    let timeout = if timeout_ms > 0 { Duration::from_millis(timeout_ms) } else { VERIFY_WAIT };
    Ok(AxJson(app.svc.verify_fresh(timeout).await?))
}

/// Makes this replica the primary of a new term: `{"replicas": [gRPC
/// URLs], "write_acks": "all", "force": false}`
/// ([`crate::service::Service::promote`]).
async fn admin_promote(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    let urls = || ApiError::bad("replicas is a list of the gRPC URLs of the copies the new primary ships to");
    let replicas = match b.get("replicas") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(a)) => a.iter().map(|v| v.as_str().map(String::from).ok_or_else(urls)).collect::<Result<_, _>>()?,
        Some(_) => return Err(urls()),
    };
    let acks = match b.get("write_acks") {
        None | Some(Json::Null) => Acks::All,
        Some(v) => v.as_str().and_then(Acks::parse).ok_or_else(|| ApiError::bad("write_acks is all, majority or primary"))?,
    };
    let force = match b.get("force") {
        None | Some(Json::Null) => false,
        Some(v) => v.as_bool().ok_or_else(|| ApiError::bad("force is true or false"))?,
    };
    Ok(AxJson(app.svc.promote(&who(w), replicas, acks, force).await?))
}

/// The gRPC URLs a request names under `key`.
fn urls(b: &Json, key: &str) -> Result<Vec<String>, ApiError> {
    let bad = || ApiError::bad(format!("{key} is a list of gRPC URLs (http://host:port)"));
    match b.get(key) {
        Some(Json::Array(a)) => a.iter().map(|v| v.as_str().filter(|s| !s.is_empty()).map(String::from).ok_or_else(bad)).collect(),
        _ => Err(bad()),
    }
}

fn body_bool(b: &Json, key: &str) -> Result<bool, ApiError> {
    match b.get(key) {
        None | Some(Json::Null) => Ok(false),
        Some(v) => v.as_bool().ok_or_else(|| ApiError::bad(format!("{key} is true or false"))),
    }
}

/// `POST /v1/admin/shards {"servers": [...], "write_acks"?}`: a new
/// shard, holding no slot until slots move to it.
async fn add_shard(State(app): State<App>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let servers = urls(&b, "servers")?;
    let acks = match b.get("write_acks") {
        None | Some(Json::Null) => Acks::All,
        Some(v) => v.as_str().and_then(Acks::parse).ok_or_else(|| ApiError::bad("write_acks is all, majority or primary"))?,
    };
    let j = match app.svc.coordinator()?.add_shard(servers.clone(), acks).await {
        Ok(j) => j,
        Err(e) => return Ok(tried(format!("add a shard served by {}", servers.join(", ")))(e)),
    };
    let detail = format!("added shard {} served by {}", j["shard"], servers.join(", "));
    Ok(audited(j, detail))
}

/// `DELETE /v1/admin/shards/:i`: removes a shard that holds no slot.
async fn remove_shard(State(app): State<App>, axum::extract::Path(i): axum::extract::Path<usize>) -> Result<Response, ApiError> {
    keys::note_change(keys::Change { object: Some(format!("shard {i}")), ..Default::default() });
    let j = match app.svc.coordinator()?.remove_shard(i).await {
        Ok(j) => j,
        Err(e) => return Ok(tried(format!("remove shard {i}"))(e)),
    };
    Ok(audited(j, format!("removed shard {i}")))
}

/// `POST /v1/admin/shards/:i/replicas {"url", "force"?, "accept_data_loss"?}`: one more
/// server for shard `i`, a replica of its primary.
async fn add_replica(State(app): State<App>, axum::extract::Path(i): axum::extract::Path<usize>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let url = body_str(&b, "url")?.to_string();
    keys::note_change(keys::Change { object: Some(format!("shard {i}")), ..Default::default() });
    let j = match app.svc.coordinator()?.add_replica(i, &url, body_bool(&b, "force")?, body_bool(&b, "accept_data_loss")?).await {
        Ok(j) => j,
        Err(e) => return Ok(tried(format!("add {url} to shard {i}"))(e)),
    };
    keys::note_change(keys::Change { object: Some(format!("shard {i}")), after: Some(json!({"added": url})), ..Default::default() });
    Ok(audited(j, format!("added {url} to shard {i}")))
}

/// `POST /v1/admin/shards/:i/promote {"url", "force"?}`: makes that
/// replica shard `i`'s primary, by a majority of the shard's votes.
async fn promote_in_shard(State(app): State<App>, axum::extract::Path(i): axum::extract::Path<usize>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let url = body_str(&b, "url")?.to_string();
    let c = app.svc.coordinator()?;
    // The copy by its short name (`s0-1`), as the cluster names it.
    let name = copy_of(&*c.snapshot().await, &url).map_or_else(|_| url.clone(), |c| c.short);
    let force = body_bool(&b, "force")?;
    // cluster2: the votes a refusal quotes, in words (NEW-cluster-admin-3).
    keys::note_change(keys::Change { object: Some(format!("shard {i}")), ..Default::default() });
    let j = match c.promote(i, &url, force).await {
        Ok(j) => j,
        Err(e) => return Ok(tried(format!("make {name} the primary of shard {i}{}", if force { " (forced)" } else { "" }))(ApiError { message: crate::coord::plan::plain_votes(&e.message), ..e })),
    };
    keys::note_change(keys::Change { object: Some(format!("shard {i}")), after: Some(json!({"primary": url, "copy": name, "forced": force})), ..Default::default() });
    Ok(audited(j, format!("made {name} the primary of shard {i}{}", if force { " (forced, asking no votes)" } else { "" })))
}

/// `DELETE /v1/admin/shards/:i/replicas?url=&accept_data_loss=`: one server fewer for shard
/// `i`.
async fn remove_replica(State(app): State<App>, axum::extract::Path(i): axum::extract::Path<usize>, Query(q): Params) -> Result<Response, ApiError> {
    let url = q.get("url").filter(|u| !u.is_empty()).ok_or_else(|| ApiError::bad("url is the gRPC URL of the server to remove (?url=http://host:port)"))?;
    keys::note_change(keys::Change { object: Some(format!("shard {i}")), ..Default::default() });
    let accept = q.get("accept_data_loss").is_some_and(|v| v == "true");
    let j = match app.svc.coordinator()?.remove_replica(i, url, accept).await {
        Ok(j) => j,
        Err(e) => return Ok(tried(format!("remove {url} from shard {i}"))(e)),
    };
    keys::note_change(keys::Change { object: Some(format!("shard {i}")), before: Some(json!({"removed": url})), ..Default::default() });
    Ok(audited(j, format!("removed {url} from shard {i}")))
}

/// The most slots each move of a rebalance or drain takes: the body's
/// `"chunk"`, or [`crate::coord::MOVE_SLOTS`].
fn chunk(b: &Option<AxJson<Json>>) -> Result<usize, ApiError> {
    match b.as_ref().and_then(|b| b.get("chunk")) {
        None | Some(Json::Null) => Ok(crate::coord::MOVE_SLOTS),
        Some(v) => v.as_u64().filter(|&c| (1..=crate::slots::SLOTS as u64).contains(&c)).map(|c| c as usize).ok_or_else(|| ApiError::bad(format!("chunk is the most slots one move takes: 1 to {}", crate::slots::SLOTS))),
    }
}

/// `POST /v1/admin/rebalance {"chunk"?}`: evens out the slots over the
/// shards, in the background (`GET /v1/admin/moves` follows it).
async fn rebalance(State(app): State<App>, w: Option<axum::Extension<Principal>>, body: Option<AxJson<Json>>) -> Result<Response, ApiError> {
    let j = match app.svc.coordinator()?.rebalance(chunk(&body)?) {
        Ok(j) => j,
        Err(e) => return Ok(tried("even the slots out over the shards".into())(e)),
    };
    let title = format!("Slots even out over the shards: {} moves", j["job"]["of"]);
    let j = moves_started(&app, w, j, title, None);
    let detail = format!("rebalancing: {} moves", j["job"]["of"]);
    // cluster2: its event is the moves' (NEW-scenarios-15).
    Ok(evented(audited(j, detail)))
}

/// `POST /v1/admin/shards/:i/drain {"chunk"?}`: moves every slot of shard
/// `i` to the others, in the background, so that it can be removed.
async fn drain_shard(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(i): axum::extract::Path<usize>, body: Option<AxJson<Json>>) -> Result<Response, ApiError> {
    keys::note_change(keys::Change { object: Some(format!("shard {i}")), ..Default::default() });
    let j = match app.svc.coordinator()?.drain(i, chunk(&body)?) {
        Ok(j) => j,
        Err(e) => return Ok(tried(format!("drain shard {i}"))(e)),
    };
    let title = format!("Shard {i} drains: {} moves", j["job"]["of"]);
    let j = moves_started(&app, w, j, title, Some(i));
    let detail = format!("draining shard {i}: {} moves", j["job"]["of"]);
    Ok(evented(audited(j, detail)))
}

/// A job of moves just started: who started it, and its event.
fn moves_started(app: &App, w: Option<axum::Extension<Principal>>, j: Json, title: String, shard: Option<usize>) -> Json {
    let by = keys::audit_name(&who(w));
    let Ok(c) = app.svc.coordinator() else { return j };
    c.moves_by(by.clone());
    let where_ = shard.map_or(Json::Null, |s| json!({"shard": s}));
    app.svc.events.add(crate::events::Event { kind: "moves_started".into(), severity: "info".into(), title, detail: json!({"kind": j["job"]["kind"], "of": j["job"]["of"]}), where_, who: Some(by) });
    c.moves()
}

/// `POST /v1/admin/plan {"shards": [{"shard", "copies": [urls]}],
/// "add_shards": n, "remove_shards": [i], "moves": [{"slots": [a, b],
/// "to": i}]}` on a coordinator of its own machines: the steps that
/// would make the cluster so, the rows they copy and how long they take,
/// what clients feel meanwhile, what to check first, and the copies they
/// remove. Nothing changes. A plan holds only what was asked: a shard not
/// named keeps its copies.
async fn admin_plan(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let c = app.svc.coordinator()?;
    let snap = c.snapshot().await;
    let mut p = crate::coord::plan::plan_at(&snap, &b, c.moves()["running"] == true, c.measured_rate())?;
    // cluster2: each server the plan adds is asked whether it answers, is
    // empty, runs the cluster's schema and has room (NEW-cluster-admin-10).
    c.probe_new(&mut p).await;
    Ok(AxJson(p))
}

/// The person a request is, for a plan applied on their behalf.
fn plan_who(w: Option<axum::Extension<Principal>>) -> crate::coord::plan::Who {
    let p = who(w);
    let caller = keys::caller().unwrap_or_default();
    crate::coord::plan::Who { by: keys::audit_name(&p), key: p.key.clone(), client: caller.named(), addr: caller.addr, reason: caller.reason }
}

/// `POST /v1/admin/plan/apply {"steps": [...], "estimate_s"?}`: applies a
/// plan on this coordinator, its steps in order, in the background
/// (`GET /v1/admin/plan/run` follows it); or, with the body of `POST
/// /v1/admin/plan` and no steps, the plan that answers, if its checks pass.
async fn plan_apply(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(mut b): AxJson<Json>) -> Result<Response, ApiError> {
    if b.get("steps").is_none() {
        let c = app.svc.coordinator()?;
        let snap = c.snapshot().await;
        let mut p = crate::coord::plan::plan_at(&snap, &b, c.moves()["running"] == true, c.measured_rate())?;
        c.probe_new(&mut p).await;
        if p["ok"] != true {
            let why: Vec<&str> = p["preflight"].as_array().into_iter().flatten().filter(|x| x["ok"] == false).filter_map(|x| x["text"].as_str()).collect();
            return Err(ApiError { code: None, status: 409, message: format!("the plan's checks fail: {}", why.join("; ")) });
        }
        b = json!({"steps": p["steps"], "estimate_s": p["estimate_s"]});
    }
    // One plan at a time in the cluster: not while another coordinator
    // applies one.
    if let Some(why) = crate::coord::plan::busy_elsewhere(&plans_elsewhere(&app).await) {
        return Err(ApiError { code: None, status: 409, message: why });
    }
    let j = crate::coord::plan::apply(&app.svc, &b, plan_who(w))?;
    let r = &j["run"];
    let texts: Vec<&str> = r["steps"].as_array().into_iter().flatten().filter_map(|x| x["text"].as_str()).collect();
    keys::note_change(keys::Change { object: Some(format!("plan {}", r["id"].as_str().unwrap_or("?"))), after: Some(json!({"plan": r["id"], "steps": texts})), ..Default::default() });
    let detail = format!("applied a plan of {} {} ({}): {}", texts.len(), if texts.len() == 1 { "step" } else { "steps" }, r["id"].as_str().unwrap_or("?"), texts.join("; "));
    Ok(audited(j, detail.chars().take(1500).collect()))
}

/// `GET /v1/admin/simulator`: the test run under way or last, the reports
/// kept, and what a run can be given (presets, kinds, machines).
async fn sim_view(State(app): State<App>) -> Res {
    Ok(AxJson(crate::traffic::run::view(&app.svc)))
}

/// `POST /v1/admin/simulator {profile?, mix?, docs?, shape?, ...}`: starts
/// a test run: it loads a table of test documents, measures each kind of
/// request alone, sends the mix at a rising (or a steady) rate, and fits a
/// model to it ([`crate::traffic::run::Config`]).
async fn sim_start(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let config: crate::traffic::run::Config = serde_json::from_value(b).map_err(|e| ApiError::bad(format!("a test run's settings: {e}")))?;
    let p = who(w);
    let j = crate::traffic::run::start(&app.svc, &keys::audit_name(&p), config)?;
    let detail = format!("started a simulator test run ({})", j["id"].as_str().unwrap_or("?"));
    Ok(audited(j, detail))
}

/// `POST /v1/admin/simulator/stop`: the run stops after its requests under way.
async fn sim_stop(State(app): State<App>) -> Result<Response, ApiError> {
    let j = crate::traffic::run::stop(&app.svc);
    Ok(audited(j, "stopped the simulator's test run".into()))
}

/// `POST /v1/admin/simulator/predict`: other shapes, by a report's model.
async fn sim_predict(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    Ok(AxJson(crate::traffic::run::predict(&app.svc, &b)?))
}

/// `GET /v1/admin/simulator/runs/:id`: a report kept.
async fn sim_report(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    Ok(AxJson(crate::traffic::run::report(&app.svc, &id)?))
}

/// `DELETE /v1/admin/simulator/table`: drops the test table a run kept.
async fn sim_drop(State(app): State<App>) -> Result<Response, ApiError> {
    let j = crate::traffic::run::drop_table(&app.svc).await?;
    Ok(audited(j, format!("dropped the simulator's test table {}", crate::traffic::data::TABLE)))
}

/// `GET /v1/admin/plan/run`: the plan this coordinator applies or applied
/// last, step by step; and, under `elsewhere`, the plan each other
/// coordinator of the cluster applies or applied, so that every
/// coordinator's Cluster page shows it (asked of a peer, only its own).
async fn plan_run(State(app): State<App>, headers: HeaderMap) -> Res {
    app.svc.coordinator()?;
    let mut j = crate::coord::plan::view(&app.svc);
    if crate::peers::from_peer(&headers).is_none() {
        j["elsewhere"] = plans_elsewhere(&app).await;
    }
    Ok(AxJson(j))
}

/// The plan of each other coordinator: `[{coordinator, url, run}]`, or the
/// `error` asking it gave.
async fn plans_elsewhere(app: &App) -> Json {
    let got = crate::peers::get_all(&app.svc, "/v1/admin/plan/run").await;
    Json::Array(got.into_iter().map(|(p, r)| crate::coord::plan::elsewhere(&p.name, &p.url, r)).collect())
}

/// `POST /v1/admin/plan/run/stop`: the plan stops after its step under way.
async fn plan_stop(State(app): State<App>) -> Result<Response, ApiError> {
    let j = crate::coord::plan::stop(&app.svc)?;
    keys::note_change(keys::Change { object: Some(format!("plan {}", j["run"]["id"].as_str().unwrap_or("?"))), ..Default::default() });
    Ok(audited(j, "stopping the plan being applied".into()))
}

/// `POST /v1/admin/plan/run/resume`: applies a plan that stopped (a
/// restart, a failed step, Stop) again from the step it stopped at.
async fn plan_resume(State(app): State<App>, w: Option<axum::Extension<Principal>>) -> Result<Response, ApiError> {
    let j = crate::coord::plan::resume(&app.svc, plan_who(w).by)?;
    keys::note_change(keys::Change { object: Some(format!("plan {}", j["run"]["id"].as_str().unwrap_or("?"))), ..Default::default() });
    let detail = format!("resumed plan {} at step {} of {}", j["run"]["id"].as_str().unwrap_or("?"), j["run"]["at"].as_u64().unwrap_or(0) + 1, j["run"]["of"]);
    Ok(audited(j, detail))
}

/// `POST /v1/admin/plan/run/discard`: forgets a plan that stopped.
async fn plan_discard(State(app): State<App>) -> Result<Response, ApiError> {
    let j = crate::coord::plan::discard(&app.svc)?;
    let d = &j["discarded"];
    keys::note_change(keys::Change { object: Some(format!("plan {}", d["id"].as_str().unwrap_or("?"))), ..Default::default() });
    let detail = format!("discarded plan {} ({}, {} of {} steps done)", d["id"].as_str().unwrap_or("?"), d["state"].as_str().unwrap_or("?"), d["done"], d["of"]);
    Ok(audited(j, detail))
}

/// `GET /v1/admin/moves`: the job of moves running or last run here, and
/// the move under way in the cluster map; under `elsewhere`, the jobs other
/// coordinators run now (asked of a peer, only its own).
async fn moves(State(app): State<App>, headers: HeaderMap) -> Res {
    let mut j = app.svc.coordinator()?.moves();
    if crate::peers::from_peer(&headers).is_none() {
        j["elsewhere"] = moves_elsewhere(&app).await;
    }
    Ok(AxJson(j))
}

/// The jobs of moves other coordinators run now: `[{coordinator, url, job}]`.
async fn moves_elsewhere(app: &App) -> Json {
    let got = crate::peers::get_all(&app.svc, "/v1/admin/moves").await;
    Json::Array(got.into_iter().filter_map(|(p, r)| r.ok().filter(|j| j["running"] == true).map(|j| json!({"coordinator": p.name, "url": p.url, "job": j["job"]}))).collect())
}

/// `POST /v1/admin/moves {"slots": [[start, end], ...], "to": shard}`:
/// moves slots of one shard to another, in the background.
async fn move_slots(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let bad = || ApiError::bad("slots is a list of slots or of [start, end) ranges of them, all held by one shard");
    let mut slots = Vec::new();
    for v in b.get("slots").and_then(Json::as_array).ok_or_else(bad)? {
        match (v.as_u64(), v.as_array().map(|r| r.iter().map(Json::as_u64).collect::<Vec<_>>())) {
            (Some(s), _) => slots.push(s as usize),
            (_, Some(r)) if matches!(r[..], [Some(a), Some(b)] if a < b && b <= crate::slots::SLOTS as u64) => slots.extend(r[0].unwrap_or(0) as usize..r[1].unwrap_or(0) as usize),
            _ => return Err(bad()),
        }
    }
    let to = b.get("to").and_then(Json::as_u64).ok_or_else(|| ApiError::bad("to is the shard the slots move to"))? as usize;
    let j = match app.svc.coordinator()?.start_move(slots.clone(), to) {
        Ok(j) => j,
        Err(e) => return Ok(tried(format!("move {} slots to shard {to}", slots.len()))(e)),
    };
    let j = moves_started(&app, w, j, format!("{} slots move to shard {to}", slots.len()), Some(to));
    Ok(evented(audited(j, format!("moving {} slots to shard {to}", slots.len()))))
}

/// `POST /v1/admin/moves/stop`: the job running here stops once its move
/// under way ends.
async fn stop_moves(State(app): State<App>) -> Result<Response, ApiError> {
    Ok(audited(app.svc.coordinator()?.stop_moves(), "stopping the job of moves".into()))
}

/// `POST /v1/admin/moves/finish`: ends the move a stopped coordinator
/// left in the cluster map (completed if it had flipped, given up if not).
async fn finish_move(State(app): State<App>) -> Result<Response, ApiError> {
    let c = app.svc.coordinator()?;
    // Not a move another coordinator is making: finishing it would give up
    // its copy under it.
    if let Some(why) = crate::coord::plan::moving_elsewhere(&moves_elsewhere(&app).await) {
        return Err(ApiError { code: None, status: 409, message: why });
    }
    let j = c.finish_move().await?;
    let detail = format!("finished the move left in the cluster map: {}", j["finished"]);
    Ok(audited(j, detail))
}

/// How long `POST /v1/admin/verify` waits for the replicas' digests by
/// default.
const VERIFY_WAIT: Duration = Duration::from_secs(60);

async fn agent_run(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    let req = agent::AgentReq {
        question: body_str(&b, "question")?.to_string(),
        model: b.get("model").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
        effort: b.get("effort").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
        api_key: b.get("api_key").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from),
        max_steps: b.get("max_steps").and_then(|v| v.as_u64()).map(|n| n.clamp(1, 20) as usize).unwrap_or(8),
        who: Some(who(w)),
    };
    app.svc.metrics.agent_run();
    Ok(AxJson(agent::run(&app.svc, req).await?))
}

/// `POST /v1/mcp`: the Model Context Protocol, whose tools call this
/// server's routes with the caller's credential ([`crate::mcp`]).
async fn mcp(State(app): State<App>, w: Option<axum::Extension<Principal>>, peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>, headers: HeaderMap, body: Bytes) -> Response {
    let who = w.map(|axum::Extension(p)| p);
    crate::mcp::serve(&app.svc, app.mcp.clone(), crate::mcp::Caller { headers: &headers, peer: peer.map(|c| c.0), who: who.as_ref() }, &body).await
}

/// `POST /v1/admin/oncall`: the on-call agent looks into an alert or a
/// question, and proposes actions ([`crate::oncall`]).
async fn oncall_run(State(app): State<App>, w: Option<axum::Extension<Principal>>, peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>, headers: HeaderMap, AxJson(b): AxJson<Json>) -> Res {
    let opt = |k: &str| b.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let req = crate::oncall::OncallReq {
        question: opt("question"),
        alert: opt("alert"),
        follows: opt("follows"),
        wait: b.get("wait").and_then(Json::as_bool).unwrap_or(true),
        model: opt("model"),
        effort: opt("effort"),
        api_key: opt("api_key"),
        max_steps: b.get("max_steps").and_then(|v| v.as_u64()).map(|n| (n as usize).clamp(1, crate::oncall::MAX_STEPS)).unwrap_or(crate::oncall::STEPS),
        note: b.get("note").and_then(Json::as_bool).unwrap_or(false),
        by: by(&who(w.clone())),
        who: Some(who(w)),
    };
    app.svc.metrics.agent_run();
    Ok(AxJson(crate::oncall::run(&app.svc, app.mcp.clone(), &headers, peer.map(|c| c.0), req).await?))
}

/// `GET /v1/admin/oncall`: whether the on-call agent can run here, its
/// defaults, and its runs in short.
async fn oncall_overview(State(app): State<App>) -> Res {
    let d = &app.svc.oncall;
    d.ready(&app.svc);
    let (running, pending) = d.counts();
    Ok(AxJson(json!({
        "configured": app.svc.agent.api_key.is_some(),
        "model": app.svc.agent.model,
        "steps": crate::oncall::STEPS,
        "max_steps": crate::oncall::MAX_STEPS,
        "kept": crate::oncall::RUNS,
        "proposal_for_ms": crate::oncall::PROPOSAL_FOR_MS,
        "runs": d.runs(),
        "running": running,
        "pending": pending,
    })))
}

/// `GET /v1/admin/oncall/runs`: the runs kept, newest first, in short.
async fn oncall_runs(State(app): State<App>) -> Res {
    app.svc.oncall.ready(&app.svc);
    Ok(AxJson(json!({"runs": app.svc.oncall.runs()})))
}

/// `GET /v1/admin/oncall/runs/:id`: one run in full, as it goes.
async fn oncall_run_get(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    app.svc.oncall.ready(&app.svc);
    app.svc.oncall.run_json(&id).map(AxJson).ok_or_else(|| ApiError { code: None, status: 404, message: format!("no run {id}: the server keeps the latest {}", crate::oncall::RUNS) })
}

/// `POST /v1/admin/oncall/runs/:id/stop`: stops a run before its next
/// model call.
async fn oncall_stop(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    app.svc.oncall.ready(&app.svc);
    Ok(AxJson(app.svc.oncall.stop(&id, &by(&who(w)))?))
}

/// `GET /v1/admin/oncall/proposals?state=&run=`: the on-call agent's
/// proposals, newest first.
async fn oncall_proposals(State(app): State<App>, Query(q): Params) -> Res {
    app.svc.oncall.ready(&app.svc);
    let pick = |k: &str| q.get(k).map(String::as_str).filter(|s| !s.is_empty());
    Ok(AxJson(json!({"proposals": app.svc.oncall.list(pick("state"), pick("run"))})))
}

/// `POST /v1/admin/oncall/proposals/:id/approve`: makes the proposal's
/// request, as the approver.
async fn oncall_approve(State(app): State<App>, w: Option<axum::Extension<Principal>>, peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>, headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    let w = who(w);
    let agent = crate::ops::is_agent(&w, keys::caller().as_ref());
    let p = crate::oncall::approve(&app.svc, app.mcp.clone(), &headers, peer.map(|c| c.0), &id, &by(&w), agent).await?;
    keys::note(format!("approved on-call proposal {id}: {}", p["what"].as_str().unwrap_or_default()));
    Ok(AxJson(p))
}

/// The router [`crate::mcp::forward`] calls, for dry runs.
fn routes_of(app: &App) -> Result<axum::Router, ApiError> {
    app.mcp.clone().ok_or_else(|| ApiError::internal("the server's own routes are missing"))
}

/// `POST /v1/admin/changes/preview {method, path, body?}`: the dry run of
/// any change ([`crate::changes::preview`]).
async fn changes_preview(State(app): State<App>, w: Option<axum::Extension<Principal>>, peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>, headers: HeaderMap, AxJson(b): AxJson<Json>) -> Res {
    let w = who(w);
    let router = routes_of(&app)?;
    let a = crate::changes::asked(&b)?;
    let caller = crate::mcp::Caller { headers: &headers, peer: peer.map(|c| c.0), who: Some(&w) };
    Ok(AxJson(crate::changes::preview(&app.svc, &router, &caller, &a).await?))
}

/// `POST /v1/admin/changes {method, path, body?, reason, what?}`: files a
/// change request, pending a person's approval.
async fn changes_file(State(app): State<App>, w: Option<axum::Extension<Principal>>, peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>, headers: HeaderMap, AxJson(b): AxJson<Json>) -> Res {
    let w = who(w);
    let router = routes_of(&app)?;
    let a = crate::changes::asked(&b)?;
    let caller = crate::mcp::Caller { headers: &headers, peer: peer.map(|c| c.0), who: Some(&w) };
    let reason = b["reason"].as_str().map(String::from).or_else(|| keys::caller().and_then(|c| c.reason)).unwrap_or_default();
    let agent = crate::ops::is_agent(&w, keys::caller().as_ref());
    let j = crate::changes::file(&app.svc, &router, &caller, &by(&w), agent, a, &reason, b["what"].as_str()).await?;
    keys::note(format!("filed change request {}: {}", j["id"].as_str().unwrap_or("?"), j["what"].as_str().unwrap_or_default()));
    Ok(AxJson(j))
}

/// `GET /v1/admin/changes?state=`: the change requests, newest first.
async fn changes_list(State(app): State<App>, Query(q): Params) -> Res {
    app.svc.oncall.ready(&app.svc);
    let state = q.get("state").map(String::as_str).filter(|s| !s.is_empty());
    let changes: Vec<Json> = app.svc.oncall.list(state, None).into_iter().filter(|p| p["action"] == "change").collect();
    let pending = changes.iter().filter(|p| p["state"] == "pending").count();
    Ok(AxJson(json!({"changes": changes, "pending": pending})))
}

/// `GET /v1/admin/changes/:id`: one change request (or on-call proposal).
async fn changes_get(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    app.svc.oncall.ready(&app.svc);
    app.svc.oncall.get(&id).map(AxJson).ok_or_else(|| ApiError::coded(404, "not_found", format!("no change request {id}")))
}

/// `POST /v1/admin/changes/:id/approve`: a person applies it, after it is
/// dry run again ([`crate::oncall::approve`]).
async fn changes_approve(State(app): State<App>, w: Option<axum::Extension<Principal>>, peer: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>, headers: HeaderMap, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    let w = who(w);
    let agent = crate::ops::is_agent(&w, keys::caller().as_ref());
    let p = crate::oncall::approve(&app.svc, app.mcp.clone(), &headers, peer.map(|c| c.0), &id, &by(&w), agent).await?;
    keys::note(format!("approved change request {id}: {}", p["what"].as_str().unwrap_or_default()));
    Ok(AxJson(p))
}

/// `POST /v1/admin/changes/:id/reject {reason?}`: a person turns it down.
async fn changes_reject(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, b: Option<AxJson<Json>>) -> Res {
    let w = who(w);
    if crate::ops::is_agent(&w, keys::caller().as_ref()) {
        return Err(ApiError::coded(403, "approval_refused", format!("{id} is decided by a person: an agent withdraws its own change requests (POST /v1/admin/changes/{id}/withdraw)")));
    }
    let reason = b.and_then(|AxJson(b)| b.get("reason").and_then(Json::as_str).map(String::from)).unwrap_or_default();
    app.svc.oncall.ready(&app.svc);
    let p = app.svc.oncall.reject(&id, &by(&w), &reason)?;
    keys::note(format!("rejected change request {id}: {}", p["what"].as_str().unwrap_or_default()));
    Ok(AxJson(p))
}

/// `POST /v1/admin/changes/:id/withdraw {reason?}`: whoever filed it
/// takes it back.
async fn changes_withdraw(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, b: Option<AxJson<Json>>) -> Res {
    let reason = b.and_then(|AxJson(b)| b.get("reason").and_then(Json::as_str).map(String::from)).unwrap_or_default();
    app.svc.oncall.ready(&app.svc);
    let p = app.svc.oncall.withdraw(&id, &by(&who(w)), &reason)?;
    keys::note(format!("withdrew change request {id}"));
    Ok(AxJson(p))
}

/// `GET /v1/catalog?q=`: every route with its scope, tier and tool, and
/// every error code ([`crate::catalog`]).
async fn catalog(Query(q): Params) -> AxJson<Json> {
    AxJson(crate::catalog::answer(q.get("q").map(String::as_str)))
}

/// `GET /v1/runbooks?for=`: the runbook for an alert's rule or a fleet
/// problem's id, or those that hold words ([`crate::runbooks`]).
async fn runbooks(Query(q): Params) -> AxJson<Json> {
    AxJson(crate::runbooks::find(q.get("for").or_else(|| q.get("q")).map(String::as_str).unwrap_or_default()))
}

/// `GET /v1/admin/jobs?kind=`: what runs in the background now, in one
/// shape: `{kind, id, state, running, started_ms, done, of, eta_s, url}`,
/// `url` the route that follows it (and `GET /v1/events?after=&wait_s=`
/// waits for its next event). Kinds: `plan`, `moves`, `oncall`,
/// `change`.
async fn jobs(State(app): State<App>, Query(q): Params) -> Res {
    let want = q.get("kind").map(String::as_str).filter(|s| !s.is_empty());
    let pick = |k: &str| want.is_none_or(|w| w.split(',').any(|x| x.trim() == k));
    let mut jobs: Vec<Json> = Vec::new();
    let of = |kind: &str, j: &Json, id: Json, url: &str| -> Json {
        let state = j["state"].clone();
        json!({"kind": kind, "id": id, "state": state, "running": state == "running", "started_ms": j["started_ms"], "done": j["done"], "of": j["of"], "eta_s": j["eta_s"], "url": url})
    };
    if app.svc.coordinator().is_ok() {
        if pick("plan") {
            let v = crate::coord::plan::view(&app.svc);
            if v["run"].is_object() {
                jobs.push(of("plan", &v["run"], v["run"]["id"].clone(), "/v1/admin/plan/run"));
            }
        }
        if pick("moves") {
            let m = app.svc.coordinator()?.moves();
            if m["job"].is_object() {
                jobs.push(of("moves", &m["job"], m["job"]["id"].clone(), "/v1/admin/moves"));
            }
        }
    }
    app.svc.oncall.ready(&app.svc);
    if pick("oncall") {
        for r in app.svc.oncall.runs().into_iter().filter(|r| r["state"] == "running") {
            let id = r["id"].as_str().unwrap_or_default().to_string();
            jobs.push(of("oncall", &r, json!(id), &format!("/v1/admin/oncall/runs/{id}")));
        }
    }
    if pick("change") {
        for p in app.svc.oncall.list(None, None).into_iter().filter(|p| p["action"] == "change" && (p["state"] == "pending" || p["state"] == "applying")) {
            let id = p["id"].as_str().unwrap_or_default().to_string();
            let mut j = of("change", &json!({"state": p["state"], "started_ms": p["created_ms"]}), json!(id), &format!("/v1/admin/changes/{id}"));
            j["what"] = p["what"].clone();
            j["expires_ms"] = p["expires_ms"].clone();
            jobs.push(j);
        }
    }
    let running = jobs.iter().filter(|j| j["running"] == true).count();
    Ok(AxJson(json!({"jobs": jobs, "running": running})))
}

/// `POST /v1/admin/oncall/proposals/:id/reject`: `{reason?}`.
async fn oncall_reject(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, b: Option<AxJson<Json>>) -> Res {
    let reason = b.and_then(|AxJson(b)| b.get("reason").and_then(Json::as_str).map(String::from)).unwrap_or_default();
    app.svc.oncall.ready(&app.svc);
    let p = app.svc.oncall.reject(&id, &by(&who(w)), &reason)?;
    keys::note(format!("rejected on-call proposal {id}: {}", p["what"].as_str().unwrap_or_default()));
    Ok(AxJson(p))
}

/// `GET /v1/admin/model-budget`: the model calls each API key made today,
/// by key id, and how many it may ([`crate::budget`]).
async fn model_budget(State(app): State<App>) -> Res {
    Ok(AxJson(app.svc.model_budget.json()))
}

/// `POST /v1/judge`: a Claude model's grade of each row for a query
/// ([`crate::judge`]).
async fn judge(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    let opt = |k: &str| b.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from);
    let keys = b.get("keys").and_then(|v| v.as_array()).cloned().ok_or_else(|| ApiError::bad("the body needs \"keys\", the rows' keys"))?;
    let fields = match b.get("fields") {
        None | Some(Json::Null) => Vec::new(),
        Some(v) => v.as_array().map(|a| a.iter().filter_map(|f| f.as_str().map(String::from)).collect()).ok_or_else(|| ApiError::bad("fields is a list of field names"))?,
    };
    let req = judge::JudgeReq { table: opt("table"), query: body_str(&b, "query")?.to_string(), keys, fields, model: opt("model"), api_key: opt("api_key"), who: Some(who(w)) };
    Ok(AxJson(judge::run(&app.svc, req).await?))
}

/// `POST /v1/relevance/autopilot` ([`crate::autopilot`]).
async fn autopilot_run(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    Ok(AxJson(crate::autopilot::run(&app.svc, &who(w), b).await?))
}

async fn feedback_submit(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Res {
    Ok(AxJson(crate::feedback::submit(&app.svc, &who(w), b).await?))
}

async fn feedback_list(State(app): State<App>, w: Option<axum::Extension<Principal>>, Query(q): Query<std::collections::HashMap<String, String>>) -> Res {
    Ok(AxJson(crate::feedback::list(&app.svc, &who(w), &q).await?))
}

async fn feedback_get(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, Query(q): Query<std::collections::HashMap<String, String>>) -> Res {
    let shot = q.get("shot").is_some_and(|s| s == "1" || s == "true");
    Ok(AxJson(crate::feedback::get(&app.svc, &who(w), &id, shot).await?))
}

async fn feedback_update(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, AxJson(b): AxJson<Json>) -> Res {
    Ok(AxJson(crate::feedback::update(&app.svc, &who(w), &id, b).await?))
}

async fn feedback_delete(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    Ok(AxJson(crate::feedback::delete(&app.svc, &who(w), &id).await?))
}

/// A feedback's picture, served inert as media files are ([`media_headers`]).
async fn feedback_picture(app: App, w: Option<axum::Extension<Principal>>, id: String, thumb: bool) -> Result<Response, ApiError> {
    let (mime, bytes) = crate::feedback::image(&app.svc, &who(w), &id, thumb).await?;
    let mut h = media_headers(&mime, "");
    // A triage changes the row, never its pictures; still, a private cache.
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("private, max-age=3600"));
    Ok((h, bytes).into_response())
}

async fn feedback_shot(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>) -> Result<Response, ApiError> {
    feedback_picture(app, w, id, false).await
}

async fn feedback_thumb(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>) -> Result<Response, ApiError> {
    feedback_picture(app, w, id, true).await
}

async fn autopilot_list(State(app): State<App>) -> Res {
    Ok(AxJson(crate::autopilot::list(&app.svc).await?))
}

async fn autopilot_get(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    Ok(AxJson(crate::autopilot::get(&app.svc, id).await?))
}

async fn autopilot_apply(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, b: Option<AxJson<Json>>) -> Res {
    let force = b.and_then(|AxJson(b)| b.get("force").and_then(|f| f.as_bool())).unwrap_or(false);
    Ok(AxJson(crate::autopilot::apply(&app.svc, &who(w), id, force).await?))
}

async fn autopilot_dismiss(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    Ok(AxJson(crate::autopilot::dismiss(&app.svc, &who(w), id).await?))
}

type Params = Query<std::collections::HashMap<String, String>>;

fn param_u64(q: &std::collections::HashMap<String, String>, k: &str, default: u64) -> Result<u64, ApiError> {
    q.get(k).map_or(Ok(default), |v| v.parse().map_err(|_| ApiError::bad(format!("{k} is a whole number, not {v:?}"))))
}

async fn live_queries(State(app): State<App>, headers: HeaderMap, Query(q): Params) -> Res {
    let mut j = json!({"queries": app.svc.live.list(), "running": app.svc.running()});
    // workload (alerts agent): grouped by statement shape and key.
    let groups = match q.get("group").map(String::as_str) {
        None | Some("") => false,
        Some("shape") => {
            j["groups"] = json!(app.svc.live.groups());
            true
        }
        Some(g) => return Err(ApiError::bad(format!("group is \"shape\", not {g:?}"))),
    };
    // fleet: what runs on every coordinator, unless asked for this one's.
    if crate::peers::from_peer(&headers).is_none() && q.get("scope").map(String::as_str) != Some("local") {
        j = crate::peers::gather_live(&app.svc, j, groups).await;
    }
    Ok(AxJson(j))
}

// ----- workload (alerts agent): bulk cancel and Contain rules

/// Cancels the running queries of a statement (`fingerprint`), of one key
/// (`key`) if given; the reason comes in `x-strata-reason`. Audited.
async fn cancel_queries(State(app): State<App>, headers: HeaderMap, Query(q): Params) -> Result<Response, ApiError> {
    let fp = q.get("fingerprint").map(String::as_str).filter(|f| !f.is_empty()).ok_or_else(|| ApiError::bad("say which statement: ?fingerprint= (and &key= for one key's)"))?;
    let key = q.get("key").map(String::as_str).filter(|k| !k.is_empty());
    let gone = app.svc.live.cancel_matching(fp, key);
    // fleet: on every coordinator.
    let (elsewhere, reached) = match crate::peers::from_peer(&headers) {
        None if q.get("scope").map(String::as_str) != Some("local") => crate::peers::cancel_elsewhere(&app.svc, fp, key).await,
        _ => (0, Json::Null),
    };
    let ids: Vec<u64> = gone.iter().filter_map(|q| q["id"].as_u64()).collect();
    tracing::info!(queries = ids.len(), fingerprint = %crate::obs::truncate(fp, 200), key = key.unwrap_or(""), "queries cancelled by an operator");
    let was: Vec<Json> = gone
        .iter()
        .map(|q| {
            let mut w = json!({"id": q["id"]});
            for k in ["key", "key_id", "client", "app", "addr", "started_ms"] {
                if !q[k].is_null() {
                    w[k] = q[k].clone();
                }
            }
            w
        })
        .collect();
    keys::note_change(keys::Change { object: Some(format!("queries of {}", crate::obs::truncate(fp, 200))), before: Some(json!({"statement": fp, "key": key, "queries": was})), ..Default::default() });
    let of = key.map_or(String::new(), |k| format!(" of key {k}"));
    let more = if elsewhere > 0 { format!(" (and {elsewhere} on the other coordinators)") } else { String::new() };
    Ok(audited(json!({"cancelled": ids.len() as u64 + elsewhere, "here": ids.len(), "ids": ids, "coordinators": reached}), format!("cancelled {} running {}{of}{more}: {}", ids.len(), if ids.len() == 1 { "query" } else { "queries" }, crate::obs::truncate(fp, 200))))
}

/// The Contain rules in force, the newest first: of every coordinator
/// (fleet: [`crate::contain::gathered`]), or this server's own with those
/// removed (`?local=1`, or a call from another coordinator).
async fn contain_rules(State(app): State<App>, headers: HeaderMap, Query(q): Params) -> AxJson<Json> {
    if q.get("local").is_some_and(|v| v == "1" || v == "true") || crate::peers::from_peer(&headers).is_some() {
        return AxJson(app.svc.contain.local_json());
    }
    AxJson(crate::contain::gathered(&app.svc).await)
}

/// Adds a Contain rule: audited, and an event. fleet: sent to every
/// other coordinator (`coordinators` says which took it); one sent by
/// another coordinator is taken up with its id, times and author.
async fn add_contain(State(app): State<App>, p: Option<axum::Extension<Principal>>, headers: HeaderMap, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let who = who(p);
    if let Some(from) = crate::peers::from_peer(&headers).filter(|_| who.root) {
        let Some((r, _)) = crate::contain::adopted(&app.svc, &b, &from)? else {
            return Ok(AxJson(json!({"ignored": "this coordinator holds no such rule: it ended, or was removed"})).into_response());
        };
        keys::note_change(keys::Change { object: Some(format!("contain rule {}", r.id)), after: Some(r.json()), ..Default::default() });
        return Ok(evented(audited(r.json(), format!("contain rule {} {} (from the coordinator at {from})", r.id, r.what()))));
    }
    let name = by(&who);
    // ops: an agent's rule lasts an hour at most.
    if crate::ops::is_agent(&who, keys::caller().as_ref()) {
        let asked = b.get("expires_s").and_then(Json::as_u64).unwrap_or(crate::contain::DEFAULT_EXPIRES_S);
        if asked > crate::ops::AGENT_CONTAIN_S {
            return Err(ApiError::coded(400, "limit_exceeded", format!("an agent's contain rule lasts {} s at most, not {asked}: ask a person for a longer one", crate::ops::AGENT_CONTAIN_S)));
        }
    }
    let r = app.svc.contain.add(&b, &name)?;
    tracing::info!(rule = %r.id, by = %name, "contain rule added: it {}", r.what());
    app.svc.events.add(crate::contain::event(&r, "added", Some(name)));
    keys::note_change(keys::Change { object: Some(format!("contain rule {}", r.id)), after: Some(r.json()), ..Default::default() });
    let mut j = r.json();
    j["coordinators"] = crate::contain::spread(&app.svc, &r).await;
    Ok(evented(audited(j, format!("contain rule {} {} for {} min: {}", r.id, r.what(), r.expires_ms.saturating_sub(r.created_ms) / 60_000, r.reason))))
}

/// Removes a Contain rule: audited, and an event. fleet: on every
/// coordinator; 404 when none held it.
async fn remove_contain(State(app): State<App>, p: Option<axum::Extension<Principal>>, headers: HeaderMap, Query(q): Params, axum::extract::Path(id): axum::extract::Path<String>) -> Result<Response, ApiError> {
    let who = who(p);
    let peer = crate::peers::from_peer(&headers).filter(|_| who.root);
    // Who removed it, as the coordinator it was removed on says.
    let name = match (&peer, q.get("by")) {
        (Some(from), Some(b)) => format!("{b} (on {from})"),
        _ => by(&who),
    };
    let here = app.svc.contain.remove(&id);
    let (reach, elsewhere) = match &peer {
        Some(_) => (Json::Null, false),
        None => crate::contain::withdraw(&app.svc, &id, &name).await,
    };
    let Some(r) = here else {
        if elsewhere {
            return Ok(audited(json!({"removed": {"id": id}, "coordinators": reach}), format!("removed contain rule {id} on the other coordinators")));
        }
        return Err(ApiError { code: None, status: 404, message: format!("no contain rule {id}") });
    };
    app.svc.events.add(crate::contain::event(&r, "removed", Some(name)));
    keys::note_change(keys::Change { object: Some(format!("contain rule {id}")), before: Some(r.json()), ..Default::default() });
    Ok(evented(audited(json!({"removed": r.json(), "coordinators": reach}), format!("removed contain rule {id}: it {}", r.what()))))
}

// ----- end workload

async fn cancel_query(State(app): State<App>, Query(params): Params, axum::extract::Path(id): axum::extract::Path<String>) -> Result<Response, ApiError> {
    // fleet: a query of another coordinator (`<id>@<its name>`, as Running
    // now lists it, or `?coordinator=` its URL) is cancelled there, and
    // audited there.
    let at = match crate::peers::query_at(&app.svc, &id) {
        Some(Ok((p, n))) => Some((p, n)),
        Some(Err(e)) => return Err(ApiError { code: None, status: 404, message: e }),
        None => None,
    };
    let id: u64 = match &at {
        Some((_, n)) => *n,
        None => id.parse().map_err(|_| ApiError::bad(format!("{id} is not a query id")))?,
    };
    let other = match (at, params.get("coordinator").filter(|u| !u.is_empty() && Some(u.trim_end_matches('/')) != app.svc.peers.me())) {
        (Some((p, _)), _) => Some(p),
        (None, Some(url)) => Some(crate::peers::other_at(&app.svc, url).ok_or_else(|| ApiError { code: None, status: 404, message: format!("no coordinator at {url} is known here") })?),
        (None, None) => None,
    };
    if let Some(p) = other {
        return match crate::peers::delete(&app.svc, &p, &format!("/v1/queries/{id}")).await {
            Ok(j) => Ok(AxJson(j).into_response()),
            Err(e) if e.contains(": 404 ") => Err(ApiError { code: None, status: 404, message: format!("query {id} is not running on {}", p.name) }),
            Err(e) => Err(ApiError { code: None, status: 502, message: e }),
        };
    }
    let q = app.svc.live.list().into_iter().find(|q| q["id"] == id).unwrap_or(Json::Null);
    match app.svc.live.cancel(id) {
        true => {
            tracing::info!(query = id, "query cancelled by an operator");
            // What was cancelled: its statement, and whose it was.
            let sql: String = q["sql"].as_str().unwrap_or("").chars().take(2048).collect();
            let key = q["key"].as_str().or(q["key_name"].as_str()).map(String::from);
            let mut was = json!({"id": id, "statement": sql});
            for k in ["key", "key_id", "client", "app", "addr", "kind", "started_ms"] {
                if !q[k].is_null() {
                    was[k] = q[k].clone();
                }
            }
            keys::note_change(keys::Change { object: Some(format!("query {id}")), before: Some(was), ..Default::default() });
            let of = key.map_or(String::new(), |k| format!(" of key {k}"));
            Ok(audited(json!({"cancelled": true, "id": id}), format!("cancelled query {id}{of}: {}", sql.chars().take(200).collect::<String>())))
        }
        false => Err(ApiError { code: None, status: 404, message: format!("query {id} is not running") }),
    }
}

/// Ended queries as a list shows them: without their steps and plan
/// lines (`GET /v1/queries/finished/:id` has them), unless `full=true`.
fn finished_list(qs: Vec<Arc<telemetry::Finished>>, q: &std::collections::HashMap<String, String>) -> Json {
    match q.get("full").map(String::as_str) {
        Some("true" | "1") => json!(qs),
        _ => Json::Array(qs.iter().map(|f| f.summary()).collect()),
    }
}

async fn recent_queries(State(app): State<App>, Query(q): Params) -> Res {
    let (since, limit) = (param_u64(&q, "since", 0)?, param_u64(&q, "limit", 100)? as usize);
    Ok(AxJson(json!({"queries": finished_list(app.svc.query_log.recent(since, limit.min(telemetry::RECENT)), &q), "now_ms": telemetry::now_ms()})))
}

async fn slow_queries(State(app): State<App>, Query(q): Params) -> Res {
    let limit = param_u64(&q, "limit", 100)? as usize;
    Ok(AxJson(json!({"queries": finished_list(app.svc.query_log.slow(limit.min(telemetry::SLOW)), &q), "slow_ms": app.svc.slow_after().map(|d| d.as_secs_f64() * 1e3)})))
}

/// One ended query with its whole profile: its trace, its steps and what
/// ran ([`telemetry::Finished`]).
async fn finished_query(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<u64>) -> Res {
    match app.svc.query_log.finished(id) {
        Some(f) => Ok(AxJson(json!(f))),
        None => Err(ApiError { code: None, status: 404, message: format!("query {id} is no longer kept: the server keeps the latest {} and the slowest {}", telemetry::RECENT, telemetry::SLOW) }),
    }
}

/// `GET /v1/profile/settings?node=`: what profiling keeps now
/// ([`profiling::settings_json`]); on a coordinator, `node` (a copy's URL
/// or short name) asks that copy's.
async fn profile_settings(State(app): State<App>, Query(q): Params) -> Res {
    if let Some(peer) = settings_peer(&app, &q)? {
        // PROF-2: another coordinator's.
        let mut j = crate::peers::get(&app.svc, &peer, "/v1/profile/settings").await.map_err(|e| ApiError { code: None, status: 502, message: e })?;
        (j["coordinator"], j["name"]) = (json!(peer.url), json!(peer.name));
        return Ok(AxJson(j));
    }
    match q.get("node").filter(|n| !n.is_empty()) {
        Some(node) => copy_settings(&app, node, json!({})).await.map(|(j, _)| AxJson(j)),
        None => Ok(AxJson(profiling::settings_json(&app.svc))),
    }
}

/// The other coordinator `?coordinator=` names (its URL or name), if any.
fn settings_peer(app: &App, q: &std::collections::HashMap<String, String>) -> Result<Option<crate::peers::Peer>, ApiError> {
    let Some(c) = q.get("coordinator").map(|c| c.trim().trim_end_matches('/')).filter(|c| !c.is_empty()) else { return Ok(None) };
    crate::peers::others(&app.svc)
        .into_iter()
        .find(|p| p.url.trim_end_matches('/') == c || p.name == c)
        .map(Some)
        .ok_or_else(|| ApiError { code: None, status: 404, message: format!("no other coordinator {c:?} answers now (GET /v1/coordinators lists them)") })
}

/// A copy's profiling settings, set first from `b` when it sets any, with
/// its `node` and `short`, over the call that profiles it.
async fn copy_settings(app: &App, node: &str, b: Json) -> Result<(Json, String), ApiError> {
    let crate::service::Backend::Cluster(c) = &app.svc.backend else {
        return Err(ApiError::bad("node names a copy of a coordinator's cluster; this server answers its own settings without it"));
    };
    let copy = copy_of(&*c.snapshot().await, node)?;
    let mut j = ask_copy(c, &copy.url, profiling::Kind::Settings, b).await?;
    (j["node"], j["short"]) = (json!(copy.url), json!(copy.short));
    Ok((j, copy.short))
}

/// `{"slow_ms": 50}` keeps every query that takes 50 ms or longer with
/// its profile among the slow ones (`null`: none, `0`: every query);
/// `{"cpu_all": true}` measures every query's CPU time, step by step.
/// Either may come alone. Until the server restarts. On a coordinator,
/// `?node=` sets a copy's, `?coordinator=` another coordinator's (PROF-2:
/// the command center sets every server's).
async fn put_profile_settings(State(app): State<App>, w: Option<axum::Extension<Principal>>, Query(q): Params, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    if let Some(peer) = settings_peer(&app, &q)? {
        // PROF-2: another coordinator's, set there (and audited there too).
        if !b.is_object() {
            return Err(ApiError::bad("the body sets slow_ms (milliseconds, or null) or cpu_all (true or false)"));
        }
        let mut j = crate::peers::put(&app.svc, &peer, "/v1/profile/settings", &b).await.map_err(|e| ApiError { code: None, status: 502, message: e })?;
        (j["coordinator"], j["name"]) = (json!(peer.url), json!(peer.name));
        let did = [b.get("slow_ms").map(|v| format!("slow_ms {}", if v.is_null() { "off".to_string() } else { v.to_string() })), b.get("cpu_all").map(|v| format!("cpu_all {v}"))];
        return Ok(audited(j, format!("profiling on coordinator {}: {}", peer.name, did.into_iter().flatten().collect::<Vec<_>>().join(", "))));
    }
    if let Some(node) = q.get("node").filter(|n| !n.is_empty()) {
        let mut asked = b.clone();
        if !asked.is_object() {
            return Err(ApiError::bad("the body sets slow_ms (milliseconds, or null) or cpu_all (true or false)"));
        }
        asked["by"] = json!(keys::audit_name(&who(w)));
        let (j, short) = copy_settings(&app, node, asked).await?;
        keys::note_change(keys::Change { object: Some(format!("profiling settings of {short}")), after: Some(j.clone()), ..Default::default() });
        let did = [b.get("slow_ms").map(|v| format!("slow_ms {}", if v.is_null() { "off".to_string() } else { v.to_string() })), b.get("cpu_all").map(|v| format!("cpu_all {v}"))];
        return Ok(audited(j, format!("profiling on {short}: {}", did.into_iter().flatten().collect::<Vec<_>>().join(", "))));
    }
    let did = profiling::set_settings(&app.svc, &b)?;
    tracing::info!(settings = %did.join(", "), "profiling settings changed");
    Ok(audited(profiling::settings_json(&app.svc), format!("profiling: {}", did.join(", "))))
}

/// A profile ([`profiling`]) of this server, or with `?node=` on a
/// coordinator, of that server of its cluster (one of `GET /v1/cluster`'s
/// replica URLs, over gRPC: [`crate::coord::Coordinator::profile`]).
/// `?format=folded` (`&value=in_use`) answers folded lines to download.
async fn profile(app: &App, kind: profiling::Kind, q: &std::collections::HashMap<String, String>, body: Json) -> Result<Response, ApiError> {
    note_profile(kind, q.get("node").map(|n| n.trim()).filter(|n| !n.is_empty()), &body);
    let asked = profiling::Asked { kind, body, folded: q.get("format").map(String::as_str) == Some("folded"), in_use: q.get("value").map(String::as_str) == Some("in_use") };
    let answer = match q.get("node").map(|n| n.trim()).filter(|n| !n.is_empty()) {
        Some(node) => app.svc.profile_node(node, asked).await?,
        None => profiling::take(&app.svc, asked).await?,
    };
    Ok(match answer {
        profiling::Answer::Json(j) => AxJson(j).into_response(),
        profiling::Answer::Folded(file, text) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8".to_string()), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{file}\""))], text).into_response(),
    })
}

/// workdata: what a CPU or allocation profile's audit entry says: of which
/// server, at what rate (`hz`, or bytes between samples), for how long,
/// and the query it ran in a loop.
fn note_profile(kind: profiling::Kind, node: Option<&str>, b: &Json) {
    let (what, rate) = match kind {
        profiling::Kind::Cpu => ("CPU", json!({"hz": b["hz"].as_i64().unwrap_or(99)})),
        profiling::Kind::Heap => ("allocation", json!({"every_bytes": b["every"].as_u64().unwrap_or(crate::heap::EVERY_DEFAULT)})),
        _ => return,
    };
    let of = node.unwrap_or("this server");
    let seconds = b["seconds"].as_f64().unwrap_or(5.0);
    let sql = b["sql"].as_str().map(|s| obs::truncate(s, 200));
    let clients = sql.as_ref().map(|_| b["clients"].as_u64().unwrap_or(1));
    let mut after = json!({"node": of, "seconds": seconds, "sql": sql, "clients": clients});
    after.as_object_mut().into_iter().for_each(|o| o.extend(rate.as_object().cloned().unwrap_or_default()));
    let at = match (rate["hz"].as_i64(), rate["every_bytes"].as_u64()) {
        (Some(hz), _) => format!("{hz} Hz"),
        (_, Some(e)) => format!("a sample every {e} bytes"),
        _ => String::new(),
    };
    let load = match (&sql, clients) {
        (Some(_), Some(c)) => format!(", of a query run in a loop by {c} {}", if c == 1 { "caller" } else { "callers" }),
        _ => String::new(),
    };
    keys::note_change(keys::Change { object: Some(format!("{what} profile of {of}")), after: Some(after), ..Default::default() });
    keys::note(format!("{} {what} profile of {of}: {seconds} s at {at}{load}", if what == "CPU" { "a" } else { "an" }));
}

/// The body of a profile with who asked for it (`by`), which the server
/// that takes it names to a second asker.
fn profile_by(mut b: Json, w: Option<axum::Extension<Principal>>) -> Json {
    if b.is_null() || b.is_object() {
        b["by"] = json!(keys::audit_name(&who(w)));
    }
    b
}

/// A CPU profile ([`crate::profiler`]): of the whole server for `seconds`
/// (default 5), or of `sql` run again and again by `clients` callers. It
/// answers the profile with how busy the server was (`cores`), the samples
/// delivered a CPU second (`rate`) and, for a query, its runs and their
/// times.
async fn cpu_profile(State(app): State<App>, w: Option<axum::Extension<Principal>>, Query(q): Params, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    profile(&app, profiling::Kind::Cpu, &q, profile_by(b, w)).await
}

/// The heap's counts, the process's resident memory, and its threads.
async fn memory(State(app): State<App>, Query(q): Params) -> Result<Response, ApiError> {
    profile(&app, profiling::Kind::Memory, &q, Json::Null).await
}

/// An allocation profile ([`crate::heap::take`]), of the whole server or of a
/// query in a loop as [`cpu_profile`].
async fn heap_profile(State(app): State<App>, w: Option<axum::Extension<Principal>>, Query(q): Params, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    profile(&app, profiling::Kind::Heap, &q, profile_by(b, w)).await
}

async fn threads(State(app): State<App>, Query(q): Params) -> Result<Response, ApiError> {
    profile(&app, profiling::Kind::Threads, &q, Json::Null).await
}

async fn request_queries(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> AxJson<Json> {
    AxJson(json!({"queries": app.svc.query_log.by_request(&id)}))
}

// ----- workload (alerts agent): statements over a window

/// The window `from`/`to` (ms) a request names, by default the last 15
/// minutes ([`telemetry::window_of`]).
fn window_param(q: &std::collections::HashMap<String, String>) -> Result<(u64, u64), ApiError> {
    let at = |k: &str| param_u64(q, k, 0).map(|v| (v > 0).then_some(v));
    Ok(telemetry::window_of(at("from")?, at("to")?))
}

/// The statements of a window, by time spent, each with the window before
/// (`prev`), its keys and whether it is new ([`telemetry::Statements`]).
async fn query_stats(State(app): State<App>, Query(q): Params, headers: HeaderMap) -> Res {
    let (from, to) = window_param(&q)?;
    let limit = param_u64(&q, "limit", 200)? as usize;
    let st = &app.svc.query_log.statements;
    // R2-workload-1: another coordinator asks for this one's counts, to merge.
    if q.get("raw").is_some_and(|v| v == "1" || v == "true") {
        return Ok(AxJson(st.window_raw(from, to, q.get("fingerprint").map(String::as_str))));
    }
    let local = q.get("local").is_some_and(|v| v == "1" || v == "true") || crate::peers::from_peer(&headers).is_some();
    let Some(me) = crate::peers::this(&app.svc).filter(|_| !local) else { return Ok(AxJson(st.window(from, to, limit))) };
    // Every coordinator's statements, merged by shape: what a client sent
    // through any of them is here, each saying how many calls each took.
    let answers = crate::peers::get_all(&app.svc, &format!("/v1/queries/stats?from={from}&to={to}&raw=1")).await;
    let others: Vec<(String, Json)> = answers.iter().filter_map(|(p, a)| a.as_ref().ok().map(|j| (p.name.clone(), j.clone()))).collect();
    let mut j = st.window_merged(from, to, limit, &others, &me.name);
    j["coordinators"] = crate::peers::reach(&app.svc, &answers);
    Ok(AxJson(j))
}

/// One statement (by its id or fingerprint) over a window: a point a
/// minute, its keys, shards, slowest calls and errors.
async fn query_stat(State(app): State<App>, axum::extract::Path(which): axum::extract::Path<String>, Query(q): Params, headers: HeaderMap) -> Res {
    let (from, to) = window_param(&q)?;
    let st = &app.svc.query_log.statements;
    let mine = st.detail(&which, from, to);
    let missing = || ApiError { code: None, status: 404, message: format!("no statement {which:?} in the last day") };
    let local = q.get("local").is_some_and(|v| v == "1" || v == "true") || crate::peers::from_peer(&headers).is_some();
    let Some(me) = crate::peers::this(&app.svc).filter(|_| !local) else { return mine.map(AxJson).ok_or_else(missing) };
    // R2-workload-1: what every coordinator ran of it.
    let path = format!("/v1/queries/stats/{}?from={from}&to={to}&local=1", crate::peers::encode(&which));
    // A coordinator that did not run it answers 404: it has none, not an error.
    let answers: Vec<_> = crate::peers::get_all(&app.svc, &path).await.into_iter().map(|(p, a)| (p, a.or_else(|e| if e.contains(": 404 ") { Ok(Json::Null) } else { Err(e) }))).collect();
    let theirs: Vec<(String, Json)> = answers.iter().filter_map(|(p, a)| a.as_ref().ok().filter(|j| !j.is_null()).map(|j| (p.name.clone(), j.clone()))).collect();
    let mut parts: Vec<(String, Json)> = mine.into_iter().map(|j| (me.name.clone(), j)).collect();
    parts.extend(theirs);
    let Some(fp) = parts.first().and_then(|(_, j)| j["statement"]["fingerprint"].as_str().map(String::from)) else { return Err(missing()) };
    let raws = crate::peers::get_all(&app.svc, &format!("/v1/queries/stats?from={from}&to={to}&raw=1&fingerprint={}", crate::peers::encode(&fp))).await;
    let others: Vec<(String, Json)> = raws.iter().filter_map(|(p, a)| a.as_ref().ok().map(|j| (p.name.clone(), j.clone()))).collect();
    let merged = st.window_merged(from, to, usize::MAX, &others, &me.name);
    let mut j = merged_detail(&parts);
    if let Some(s) = merged["statements"].as_array().into_iter().flatten().find(|s| s["fingerprint"] == fp.as_str()) {
        j["by_key"] = s["keys"].clone();
        j["statement"] = s.clone();
    }
    j["coordinators"] = crate::peers::reach(&app.svc, &answers);
    Ok(AxJson(j))
}

/// Statement pages of several coordinators as one (`parts`: each one's
/// name and page): the trend a minute summed, the shards' calls summed,
/// the slowest and latest calls of all (each naming its coordinator), and
/// the errors by message.
fn merged_detail(parts: &[(String, Json)]) -> Json {
    let mut j = parts.first().map(|(_, j)| j.clone()).unwrap_or_else(|| json!({}));
    if let [(name, _)] = parts {
        // One coordinator ran it: its page, each call naming it.
        for k in ["slowest", "latest"] {
            for c in j[k].as_array_mut().into_iter().flatten() {
                c["coordinator"] = json!(name);
            }
        }
        return j;
    }
    let f = |x: &Json, k: &str| x[k].as_f64().unwrap_or(0.0);
    // The trend: calls and errors added, the slowest of each minute kept.
    let mut trend: std::collections::BTreeMap<u64, Json> = std::collections::BTreeMap::new();
    let mut shards: std::collections::BTreeMap<u64, Json> = std::collections::BTreeMap::new();
    let (mut slowest, mut latest, mut errors) = (Vec::new(), Vec::new(), Vec::<Json>::new());
    for (name, d) in parts {
        for p in d["trend"].as_array().into_iter().flatten() {
            let Some(t) = p["t"].as_u64() else { continue };
            let e = trend.entry(t).or_insert_with(|| json!({"t": t, "calls": 0, "errors": 0, "mean_ms": 0.0, "max_ms": 0.0, "p99_ms": null, "p50_ms": null}));
            let (c0, c1) = (f(e, "calls"), f(p, "calls"));
            e["mean_ms"] = json!(if c0 + c1 > 0.0 { (f(e, "mean_ms") * c0 + f(p, "mean_ms") * c1) / (c0 + c1) } else { 0.0 });
            e["calls"] = json!(c0 as u64 + c1 as u64);
            e["errors"] = json!(e["errors"].as_u64().unwrap_or(0) + p["errors"].as_u64().unwrap_or(0));
            for k in ["max_ms", "p99_ms", "p50_ms"] {
                if p[k].is_number() && (e[k].is_null() || f(p, k) > f(e, k)) {
                    e[k] = p[k].clone();
                }
            }
        }
        for sh in d["by_shard"].as_array().into_iter().flatten() {
            let Some(i) = sh["shard"].as_u64() else { continue };
            let e = shards.entry(i).or_insert_with(|| json!({"shard": i, "calls": 0, "errors": 0, "mean_ms": 0.0, "p99_ms": 0.0, "max_ms": 0.0}));
            let (c0, c1) = (f(e, "calls"), f(sh, "calls"));
            e["mean_ms"] = json!(if c0 + c1 > 0.0 { (f(e, "mean_ms") * c0 + f(sh, "mean_ms") * c1) / (c0 + c1) } else { 0.0 });
            e["calls"] = json!(c0 as u64 + c1 as u64);
            e["errors"] = json!(e["errors"].as_u64().unwrap_or(0) + sh["errors"].as_u64().unwrap_or(0));
            for k in ["p99_ms", "max_ms"] {
                if f(sh, k) > f(e, k) {
                    e[k] = sh[k].clone();
                }
            }
        }
        let tag = |c: &Json| {
            let mut c = c.clone();
            c["coordinator"] = json!(name);
            c
        };
        slowest.extend(d["slowest"].as_array().into_iter().flatten().map(tag));
        latest.extend(d["latest"].as_array().into_iter().flatten().map(tag));
        for e in d["errors"].as_array().into_iter().flatten() {
            match errors.iter_mut().find(|x| x["message"] == e["message"]) {
                Some(x) => {
                    x["count"] = json!(x["count"].as_u64().unwrap_or(0) + e["count"].as_u64().unwrap_or(0));
                    x["last_ms"] = json!(x["last_ms"].as_u64().max(e["last_ms"].as_u64()));
                }
                None => errors.push(e.clone()),
            }
        }
    }
    slowest.sort_by(|a, b| f(b, "ms").total_cmp(&f(a, "ms")));
    slowest.truncate(10);
    latest.sort_by(|a, b| b["ts_ms"].as_u64().cmp(&a["ts_ms"].as_u64()));
    latest.truncate(20);
    errors.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
    j["trend"] = json!(trend.into_values().collect::<Vec<_>>());
    j["by_shard"] = json!(shards.into_values().collect::<Vec<_>>());
    j["by_shard_from_calls"] = json!(latest.len().max(slowest.len()));
    j["slowest"] = json!(slowest);
    j["latest"] = json!(latest);
    j["errors"] = json!(errors);
    j
}

/// Starts this server's statement counts again (audited; a coordinator's
/// shards keep theirs).
async fn reset_query_stats(State(app): State<App>) -> AxJson<Json> {
    app.svc.query_log.statements.reset();
    keys::note("reset this server's statement statistics");
    AxJson(json!({"reset": true}))
}

// ----- end workload

/// `GET /v1/timeseries?since=` (memory's points, every 2 s over the last
/// hour) or `?from=&to=`: memory's points when it holds `from`, else a
/// point a minute from the log store's `load` table (an hour apart past a
/// week), with `source` saying which.
async fn timeseries(State(app): State<App>, Query(q): Params) -> Res {
    let now = telemetry::now_ms();
    let interval = app.svc.series.interval.as_millis() as u64;
    if !q.contains_key("from") {
        let since = param_u64(&q, "since", 0)?;
        return Ok(AxJson(json!({"interval_ms": interval, "points": app.svc.series.since(since), "now_ms": now, "source": "memory"})));
    }
    let from = param_u64(&q, "from", 0)?;
    let to = param_u64(&q, "to", now)?;
    if to < from {
        return Err(ApiError::bad("to comes before from"));
    }
    let memory = |error: Option<&str>| {
        let points: Vec<Arc<Json>> = app.svc.series.since(from).into_iter().filter(|p| p["t"].as_u64().is_some_and(|t| t <= to)).collect();
        let mut j = json!({"interval_ms": interval, "points": points, "now_ms": now, "from_ms": from, "to_ms": to, "source": "memory"});
        if let Some(e) = error {
            j["error"] = json!(e);
        }
        j
    };
    let oldest = app.svc.series.since(0).first().and_then(|p| p["t"].as_u64());
    if oldest.is_some_and(|o| from + interval >= o) {
        return Ok(AxJson(memory(None)));
    }
    let Some(store) = &app.svc.log_store else {
        return Ok(AxJson(memory(Some("this server keeps no log store (--log-store): only what memory holds, the last hour, is known"))));
    };
    let (rows, interval) = load_rows(store, from, to, &["qps", "error_ratio", "errors_s", "client_errors_s", "timeouts_s", "rejected_s", "p50_ms", "p99_ms", "writes_s", "cpu", "cores", "mem_bytes", "disk_used_ratio"]).await?;
    Ok(AxJson(json!({"interval_ms": interval, "points": rows, "now_ms": now, "from_ms": from, "to_ms": to, "source": "load"})))
}

/// The SQL of the `load` rows from `from` to `to` (ms) with `cols`: a row
/// a minute up to a week, else a row an hour (means; the most of
/// `p99_ms`, `cores`, `mem_bytes` and `disk_used_ratio`); and the
/// interval.
fn load_sql(from: u64, to: u64, cols: &[&str]) -> (String, u64) {
    if to.saturating_sub(from) / 60_000 <= 10_000 {
        (format!("SELECT ts, {} FROM load WHERE ts >= :from AND ts <= :to ORDER BY ts LIMIT 10000", cols.join(", ")), 60_000)
    } else {
        let agg: Vec<String> = cols.iter().map(|c| if matches!(*c, "p99_ms" | "cores" | "mem_bytes" | "disk_used_ratio") { format!("MAX({c})") } else { format!("AVG({c})") }).collect();
        (format!("SELECT hour, {} FROM load WHERE ts >= :from AND ts <= :to GROUP BY hour LIMIT 10000", agg.join(", ")), 3_600_000)
    }
}

fn load_params(from: u64, to: u64) -> chloedb::query::Params {
    let us = |ms: u64| chloedb::value::Value::Timestamp(ms.min(i64::MAX as u64 / 1000) as i64 * 1000);
    chloedb::query::Params::new().set("from", us(from)).set("to", us(to))
}

/// A result of [`load_sql`] as points `{"t", col...}`, oldest first: `t`
/// the end of each minute (or hour).
fn load_points(rs: &chloedb::query::exec::ResultSet, cols: &[&str], interval: u64) -> Vec<Json> {
    use chloedb::value::Value;
    let mut points: Vec<Json> = rs
        .rows
        .iter()
        .map(|r| {
            let mut p = serde_json::Map::new();
            let t = match r.first() {
                Some(Value::Timestamp(t)) => t.div_euclid(1000) as u64,
                Some(Value::Int(h)) => (*h as u64 + 1) * interval,
                _ => 0,
            };
            p.insert("t".into(), json!(t));
            for (c, v) in cols.iter().zip(r.iter().skip(1)) {
                match v {
                    Value::Float(x) if x.is_finite() => {
                        p.insert((*c).into(), json!(x));
                    }
                    Value::Int(x) => {
                        p.insert((*c).into(), json!(x));
                    }
                    _ => {}
                }
            }
            Json::Object(p)
        })
        .collect();
    points.sort_by_key(|p| p["t"].as_u64());
    points
}

/// This server's `load` rows from `from` to `to` ([`load_sql`]).
async fn load_rows(store: &crate::logstore::LogStore, from: u64, to: u64, cols: &[&str]) -> Result<(Vec<Json>, u64), ApiError> {
    let (sql, interval) = load_sql(from, to, cols);
    let (rs, _) = store.query(&sql, &load_params(from, to), false, Duration::from_secs(10)).await?;
    Ok((load_points(&rs, cols, interval), interval))
}

/// `GET /v1/load?from=&to=&copies=all` on a coordinator: each copy's load
/// a minute (cpu, disk, p99, qps) from its own log store's `load`, 16
/// copies asked at once, all in 10 s; the copies that did not answer are
/// named.
async fn copies_load(State(app): State<App>, Query(q): Params) -> Res {
    let crate::service::Backend::Cluster(c) = &app.svc.backend else {
        return Err(ApiError::bad("/v1/load is a coordinator's: it asks each copy. A shard server's own load is /v1/timeseries?from="));
    };
    if q.get("copies").is_some_and(|v| v != "all") {
        return Err(ApiError::bad("copies is all, the one choice so far"));
    }
    let now = telemetry::now_ms();
    let to = param_u64(&q, "to", now)?;
    let from = param_u64(&q, "from", to.saturating_sub(3_600_000))?;
    if to < from {
        return Err(ApiError::bad("to comes before from"));
    }
    const COLS: [&str; 6] = ["qps", "cpu", "cores", "busy", "p99_ms", "disk_used_ratio"];
    let (sql, interval) = load_sql(from, to, &COLS);
    let params = load_params(from, to);
    let snap = c.snapshot().await;
    let mut names = std::collections::HashMap::new();
    for s in snap["shards"].as_array().into_iter().flatten() {
        for r in s["replicas"].as_array().into_iter().flatten() {
            if let Some(u) = r["url"].as_str() {
                names.insert(u.to_string(), (r["short"].clone(), r["name"].clone()));
            }
        }
    }
    let gate = Arc::new(tokio::sync::Semaphore::new(16));
    let deadline = Instant::now() + Duration::from_secs(10);
    let asks = c.replica_clients().into_iter().map(|(shard, url, mut client)| {
        let (gate, sql, params) = (gate.clone(), sql.clone(), params.clone());
        async move {
            let answer = async {
                let _held = tokio::time::timeout_at(deadline.into(), gate.acquire_owned()).await.map_err(|_| ApiError { code: None, status: 504, message: "not asked within 10 s".into() })?.map_err(ApiError::internal)?;
                let req = crate::pb::QueryRequest { sql, params: crate::grpc::params_to_pb(&params), shard: false, timeout_ms: deadline.saturating_duration_since(Instant::now()).as_millis() as u32, ..Default::default() };
                match tokio::time::timeout_at(deadline.into(), client.logs(tonic::Request::new(req))).await {
                    Err(_) => Err(ApiError { code: None, status: 504, message: "no answer within 10 s".into() }),
                    Ok(Err(s)) if s.code() == tonic::Code::Unimplemented => Err(ApiError { code: None, status: 501, message: "an older server, without a log store".into() }),
                    Ok(Err(s)) => Err(ApiError::from(s)),
                    Ok(Ok(a)) => crate::grpc::result_from_pb(a.into_inner()),
                }
            }
            .await;
            (shard, url, answer)
        }
    });
    let answers = futures::future::join_all(asks).await;
    let (mut copies, mut missing) = (Vec::new(), Vec::new());
    for (shard, url, answer) in answers {
        let (short, name) = names.get(&url).cloned().unwrap_or((Json::Null, Json::Null));
        match answer {
            Ok(rs) => copies.push(json!({"shard": shard, "url": url, "short": short, "name": name, "points": load_points(&rs, &COLS, interval)})),
            Err(e) => missing.push(json!({"shard": shard, "url": url, "short": short, "error": e.message})),
        }
    }
    Ok(AxJson(json!({"from_ms": from, "to_ms": to, "interval_ms": interval, "asked": copies.len() + missing.len(), "answered": copies.len(), "copies": copies, "missing": missing})))
}

/// OpenTelemetry logs in, over OTLP/HTTP ([`crate::otlp`]).
async fn otlp_logs(State(app): State<App>, w: Option<axum::Extension<Principal>>, headers: HeaderMap, Query(q): Params, body: axum::body::Bytes) -> Response {
    crate::otlp::http(&app.svc, &who(w), &headers, q.get("table").map(String::as_str), body).await
}

async fn logs(State(app): State<App>, Query(q): Params) -> Res {
    // A copy's: its filter, until when and by whom (its lines are in the
    // log store, `POST /v1/logs/query`).
    if let Some(node) = q.get("node").filter(|n| !n.is_empty()) {
        let j = copy_log_filter(&app, node).await?;
        return Ok(AxJson(json!({"node": j["node"], "short": j["short"], "filter": j["filter"], "filter_expires_ms": j["expires_ms"], "filter_set_by": j["set_by"], "filter_set_ms": j["set_ms"], "previous": j["previous"]})));
    }
    let (after, limit) = (param_u64(&q, "after", 0)?, param_u64(&q, "limit", 500)?.min(obs::RING_LINES as u64) as usize);
    let ring = obs::ring();
    let lines = ring.lines(after, q.get("level").map(String::as_str), q.get("request").map(String::as_str), q.get("q").map(String::as_str).filter(|t| !t.is_empty()), limit);
    let level = app.svc.log_filter_json();
    Ok(AxJson(json!({"lines": lines, "last": ring.last(), "filter": obs::log_filter(), "filter_expires_ms": level["expires_ms"], "filter_set_by": level["set_by"]})))
}

/// `GET /v1/logs/filter?node=`: the log filter, until when and by whom it
/// was set; on a coordinator, `node` (a copy's URL or short name) asks
/// that copy's.
async fn log_filter_now(State(app): State<App>, Query(q): Params) -> Res {
    match q.get("node").filter(|n| !n.is_empty()) {
        Some(node) => copy_log_filter(&app, node).await.map(AxJson),
        None => Ok(AxJson(app.svc.log_filter_json())),
    }
}

/// A copy's log filter (`{filter, expires_ms, set_by, set_ms, previous}`
/// with its `node` and `short`), asked over the call that sets it.
async fn copy_log_filter(app: &App, node: &str) -> Result<Json, ApiError> {
    let crate::service::Backend::Cluster(c) = &app.svc.backend else {
        return Err(ApiError::bad("node names a copy of a coordinator's cluster; this server answers its own log filter without it"));
    };
    let copy = copy_of(&*c.snapshot().await, node)?;
    let mut j = ask_copy(c, &copy.url, crate::profiling::Kind::LogFilter, json!({})).await?;
    (j["node"], j["short"]) = (json!(copy.url), json!(copy.short));
    Ok(j)
}

/// `PUT /v1/logs/filter?node= {"filter", "expires_s"?}`: this server's
/// log filter (or on a coordinator, a copy's, by its URL or short name)
/// for `expires_s` (30 min by default; 0: until set again), then back.
async fn log_filter(State(app): State<App>, w: Option<axum::Extension<Principal>>, Query(q): Params, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let filter = body_str(&b, "filter")?;
    let expires_s = match b.get("expires_s") {
        None | Some(Json::Null) => None,
        Some(v) => Some(v.as_u64().filter(|s| *s <= 7 * 86_400).ok_or_else(|| ApiError::bad("expires_s is a number of seconds up to a week (0: until set again)"))?),
    };
    let by = keys::audit_name(&who(w));
    let until = expires_s.unwrap_or(crate::service::LOG_FILTER_S);
    keys::note_change(keys::Change { object: Some("log filter".into()), before: Some(json!(obs::log_filter())), after: Some(json!({"filter": filter, "expires_s": until})), ..Default::default() });
    if let Some(node) = q.get("node").filter(|n| !n.is_empty()) {
        let crate::service::Backend::Cluster(c) = &app.svc.backend else {
            return Err(ApiError::bad("node names a copy of a coordinator's cluster; this server sets its own log filter without it"));
        };
        let copy = copy_of(&*c.snapshot().await, node)?;
        let mut j = ask_copy(c, &copy.url, crate::profiling::Kind::LogFilter, json!({"filter": filter, "expires_s": until, "by": by})).await?;
        (j["node"], j["short"]) = (json!(copy.url), json!(copy.short));
        // The copy's own event is on its own timeline; the coordinator's
        // says it of the copy (its `log_level`, not a change too).
        app.svc.events.add(crate::events::Event {
            kind: "log_level".into(),
            severity: "info".into(),
            title: format!("Logs at {filter} on {} for {} min", copy.short, until.div_ceil(60)),
            detail: json!({"filter": filter, "before": j["previous"], "expires_ms": j["expires_ms"], "node": copy.url}),
            where_: json!({"shard": copy.shard, "copy": copy.short}),
            who: Some(by),
        });
        return Ok(evented(audited(j, format!("log filter {filter} on {} for {} min", copy.short, until.div_ceil(60)))));
    }
    let j = app.svc.set_log_level(filter, Some(until), &by)?;
    Ok(evented(audited(j, format!("log filter {filter} for {} min", until.div_ceil(60)))))
}

/// A query of the log store: this server's, or on a coordinator every
/// server's merged ([`crate::logstore::cluster_query`]) unless `"scope":
/// "local"`.
async fn log_query(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let sql = body_str(&b, "sql")?;
    let params = app.svc.params(None, b.get("params")).await?;
    let timeout = app.svc.timeout_of(body_u64(&b, "timeout_ms")?);
    let local = match b.get("scope").and_then(|v| v.as_str()) {
        None | Some("cluster") => false,
        Some("local") => true,
        Some(other) => return Err(ApiError::bad(format!("scope is \"cluster\" or \"local\", not {other:?}"))),
    };
    let store = app.svc.log_store.as_ref();
    // R2-workload-1: another coordinator asks for this one's store's part of
    // its cluster query, in the form a shard answers.
    let partial = local && b.get("partial").and_then(Json::as_bool) == Some(true);
    match &app.svc.backend {
        crate::service::Backend::Cluster(c) if !local => {
            // The other coordinators' stores too: their own lines, and the
            // queries their clients sent.
            let ask = json!({"sql": sql, "params": b.get("params").cloned().unwrap_or(Json::Null), "timeout_ms": timeout.as_millis() as u64, "scope": "local", "partial": true});
            let svc = app.svc.clone();
            let peers = async move {
                let peers = crate::peers::others(&svc);
                let answers = futures::future::join_all(peers.iter().map(|p| {
                    let (svc, ask) = (svc.clone(), ask.clone());
                    async move {
                        let t = std::time::Instant::now();
                        let a = crate::peers::post(&svc, p, "/v1/logs/query", &ask).await;
                        (t.elapsed().as_secs_f64() * 1e3, a)
                    }
                }))
                .await;
                peers
                    .into_iter()
                    .zip(answers)
                    .map(|(p, (ms, a))| crate::logstore::PeerAnswer {
                        url: p.url.clone(),
                        name: p.name.clone(),
                        ms,
                        answer: a.map_err(|e| ApiError { code: None, status: 502, message: e }).and_then(|j| crate::logstore::from_partial_json(&j)),
                    })
                    .collect::<Vec<_>>()
            };
            Ok(AxJson(crate::logstore::cluster_query(store, c, sql, &params, timeout, peers).await?))
        }
        _ if partial => {
            let store = store.ok_or_else(crate::logstore::none_here)?;
            let (rs, took_ms) = store.query(sql, &params, true, timeout).await?;
            Ok(AxJson(crate::logstore::partial_json(&rs, took_ms)))
        }
        _ => {
            let store = store.ok_or_else(crate::logstore::none_here)?;
            let (rs, took_ms) = store.query(sql, &params, false, timeout).await?;
            let mut j = js::result(&rs, took_ms);
            j["nodes"] = json!([{"node": store.options().node, "rows": rs.rows.len(), "ms": took_ms}]);
            Ok(AxJson(j))
        }
    }
}

/// The log store: this server's (`store`, null without one), its tables
/// (`schemas`, in the short JSON form), and on a coordinator every
/// server's (`nodes`), as each answered `Health`.
async fn log_store(State(app): State<App>) -> AxJson<Json> {
    let schemas: Vec<Json> = crate::logstore::schemas().iter().map(|s| json!({"table": s.table, "short": s.to_short_json(), "sql": chloedb::query::ddl::create_sql(s)})).collect();
    let mut j = json!({"store": app.svc.log_store.as_ref().map(|s| s.status()), "schemas": schemas});
    if let crate::service::Backend::Cluster(c) = &app.svc.backend {
        let cluster = c.cluster().await;
        let nodes: Vec<Json> = cluster["shards"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|s| {
                let shard = s["shard"].clone();
                s["replicas"].as_array().cloned().unwrap_or_default().into_iter().map(move |r| json!({"shard": shard, "url": r["url"], "status": r["status"], "error": r["error"], "store": r["detail"]["log_store"]}))
            })
            .collect();
        j["nodes"] = json!(nodes);
    }
    AxJson(j)
}

async fn system(State(app): State<App>) -> AxJson<Json> {
    let mut j = app.svc.system();
    j["auth"] = json!({"token": app.token.is_some(), "cluster_token": app.cluster.is_some()});
    j["logs"] = json!({"filter": obs::log_filter()});
    AxJson(j)
}

/// The tenants this server serves, or 404 when it serves none.
fn tenants(app: &App) -> Result<Arc<crate::tenants::Tenants>, ApiError> {
    app.svc.tenants.clone().ok_or_else(|| ApiError { code: None, status: 404, message: "this server serves no tenants (start it with --tenants URL; experimental)".into() })
}

/// A tenant's call: it reads and writes the object store, and blocks.
async fn tenant_call<T: Send + 'static>(f: impl FnOnce() -> Result<T, chloedb::tenant::TenantError> + Send + 'static) -> Result<T, ApiError> {
    Ok(tokio::task::spawn_blocking(f).await.map_err(ApiError::internal)??)
}

async fn tenant_create(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let t = tenants(&app)?;
    let id = body_str(&b, "id")?.to_string();
    if !chloedb::tenant::valid_id(&id) {
        return Err(ApiError::bad("a tenant's id is 1 to 128 of A-Z a-z 0-9 . _ -, not starting with . or _"));
    }
    let not_sql = || ApiError::bad("sql is a statement or a list of statements");
    let statements: Vec<String> = match b.get("sql") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::String(s)) => vec![s.clone()],
        Some(Json::Array(a)) => a.iter().map(|s| s.as_str().map(String::from).ok_or_else(not_sql)).collect::<Result<_, _>>()?,
        Some(_) => return Err(not_sql()),
    };
    let template = match b.get("template") {
        None | Some(Json::Null) => None,
        Some(Json::String(name)) if crate::tenants::Tenants::TEMPLATES.contains(&name.as_str()) => Some(name.clone()),
        Some(_) => return Err(ApiError::bad(format!("template is one of: {}", crate::tenants::Tenants::TEMPLATES.join(", ")))),
    };
    Ok(AxJson(tenant_call(move || t.create(&id, &statements, template.as_deref())).await?))
}

/// `POST /v1/admin/tenants/:id/board/:tool`: a tool of the tenant's
/// blackboard, for the agent the body names ([`crate::board`]).
async fn tenant_board(State(app): State<App>, axum::extract::Path((id, tool)): axum::extract::Path<(String, String)>, AxJson(mut b): AxJson<Json>) -> Res {
    let t = tenants(&app)?;
    let agent = body_str(&b, "agent")?.to_string();
    if let Some(o) = b.as_object_mut() {
        o.remove("agent");
    }
    let called = tokio::task::spawn_blocking(move || -> Result<Json, ApiError> { Ok(crate::board::call(&*t.get(&id)?, &agent, &tool, &b)?) });
    Ok(AxJson(called.await.map_err(ApiError::internal)??))
}

/// `POST /v1/admin/tenants/:id/mcp?agent=NAME`: the blackboard's tools as
/// an MCP server, for the agent the URL names. A request is answered
/// with JSON; notifications alone with 202 and no body.
async fn tenant_mcp(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>, Query(q): Query<std::collections::HashMap<String, String>>, body: Bytes) -> Result<Response, ApiError> {
    let t = tenants(&app)?;
    let agent = q.get("agent").filter(|a| !a.is_empty()).cloned().ok_or_else(|| ApiError::bad("the URL names the agent: ?agent=NAME"))?;
    let answered = tokio::task::spawn_blocking(move || -> Result<(bool, Option<Json>), ApiError> { Ok(crate::board::mcp(&*t.get(&id)?, &agent, &body)) });
    Ok(match answered.await.map_err(ApiError::internal)?? {
        (true, Some(answer)) => AxJson(answer).into_response(),
        (true, None) => StatusCode::ACCEPTED.into_response(),
        (false, answer) => (StatusCode::BAD_REQUEST, AxJson(answer.unwrap_or(Json::Null))).into_response(),
    })
}

async fn tenant_list(State(app): State<App>) -> Res {
    let t = tenants(&app)?;
    Ok(AxJson(tenant_call(move || t.list()).await?))
}

async fn tenant_get(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    let t = tenants(&app)?;
    Ok(AxJson(tenant_call(move || t.describe(&id)).await?))
}

async fn tenant_sql(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>, AxJson(b): AxJson<Json>) -> Res {
    let t = tenants(&app)?;
    let sql = body_str(&b, "sql")?.to_string();
    let params = crate::json::params(b.get("params"))?;
    let (latest, objects) = (body_bool(&b, "latest")?, body_bool(&b, "objects")?);
    let mut j = tenant_call(move || t.sql(&id, &sql, &params, latest)).await?;
    if objects && j.get("columns").is_some() {
        as_objects(&mut j);
    }
    Ok(AxJson(j))
}

async fn tenant_base(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    let t = tenants(&app)?;
    Ok(AxJson(tenant_call(move || t.make_base(&id)).await?))
}

async fn tenant_delete(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    let t = tenants(&app)?;
    match tenant_call(move || t.delete(&id)).await? {
        true => Ok(AxJson(json!({"deleted": true}))),
        false => Err(ApiError { code: None, status: 404, message: "no tenant has this id".into() }),
    }
}

async fn apps_list(State(app): State<App>) -> AxJson<Json> {
    AxJson(app.svc.apps.listing().await)
}

async fn app_forward(
    State(app): State<App>,
    w: Option<axum::Extension<Principal>>,
    axum::extract::Path((name, rest)): axum::extract::Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    app.svc.apps.forward(&name, &who(w), method, &rest, uri.query(), &headers, body).await
}

async fn cluster(State(app): State<App>, Query(q): Params) -> AxJson<Json> {
    let mut j = app.svc.cluster().await;
    // fleet: `brief=1` leaves out each copy's detail, for a view that
    // shows only which copies answer (and the shell's badges).
    if q.get("brief").is_some_and(|v| v == "1" || v == "true") {
        for r in j["shards"].as_array_mut().into_iter().flatten().flat_map(|s| s["replicas"].as_array_mut().into_iter().flatten()) {
            if let Some(o) = r.as_object_mut() {
                o.remove("detail");
            }
        }
    }
    // cluster2: the plan being applied (or the last one), for every view.
    if app.svc.coordinator().is_ok() {
        j["plan"] = crate::coord::plan::view(&app.svc)["run"].clone();
    }
    AxJson(j)
}

/// `GET /v1/kube`: the `StrataCluster` that runs this coordinator on
/// Kubernetes (its spec, and the operator's status), or `managed: false`.
async fn kube(State(app): State<App>) -> Res {
    let mut j = match &app.svc.kube {
        None => json!({"managed": false}),
        Some(l) => l.get().await.map_err(|e| ApiError { code: None, status: 502, message: format!("the Kubernetes API: {e}") })?,
    };
    // core: the images the copies run, and how far a rollout is.
    if let crate::service::Backend::Cluster(c) = &app.svc.backend {
        j["versions"] = json!(copy_versions(&*c.snapshot().await));
    }
    if let Some(p) = rollout_progress(&app, &j["status"]) {
        j["progress"] = p;
    }
    Ok(AxJson(j))
}

/// The images the cluster's copies run, one entry a digest (or image,
/// or build when no image is known), the most run first.
fn copy_versions(snap: &Json) -> Vec<Json> {
    let mut by: Vec<Json> = Vec::new();
    for r in snap["shards"].as_array().into_iter().flatten().flat_map(|s| s["replicas"].as_array().into_iter().flatten()) {
        if r["status"] == "down" {
            continue;
        }
        let v = &r["version"];
        let key = v["digest"].as_str().or(v["image"].as_str()).or(v["version"].as_str()).unwrap_or("unknown").to_string();
        let short = r["short"].clone();
        match by.iter_mut().find(|e| e["key"] == key.as_str()) {
            Some(e) => {
                e["count"] = json!(e["count"].as_u64().unwrap_or(0) + 1);
                e["copies"].as_array_mut().expect("a list").push(short);
            }
            None => by.push(json!({
                "key": key, "digest": v["digest"], "short": v["digest"].as_str().map(crate::operator::upgrade::short), "image": v["image"],
                "version": v["version"], "revision": v["revision"], "count": 1, "copies": [short],
            })),
        }
    }
    by.sort_by(|a, b| b["count"].as_u64().cmp(&a["count"].as_u64()));
    for e in &mut by {
        if let Some(o) = e.as_object_mut() {
            o.remove("key");
        }
    }
    by
}

/// How far the rollout of `status.rollout` is, when one runs: copies
/// done of the total, seconds a copy so far, the time left at that pace,
/// since when the coordinator saw it run, and the step it waits on.
fn rollout_progress(app: &App, status: &Json) -> Option<Json> {
    let r = &status["rollout"];
    let total = r["copies"].as_u64()?;
    let stale = r["stale"].as_array().map_or(0, |v| v.len() as u64);
    let version = r["version"].as_str().unwrap_or_default().to_string();
    let now = telemetry::now_ms();
    let mut seen = app.svc.rollout_seen.lock().unwrap_or_else(|e| e.into_inner());
    let running = stale > 0 || r["step"].is_object();
    if !running {
        // A rollout seen to its end: its pace is the next one's estimate.
        if let Some((v, started, at_start)) = seen.take() {
            if v == version && at_start > 0 {
                *app.svc.rollout_pace_s.lock().unwrap_or_else(|e| e.into_inner()) = Some(now.saturating_sub(started) as f64 / 1e3 / at_start as f64);
            }
        }
        return None;
    }
    let (started, at_start) = match seen.as_ref() {
        Some((v, s, n)) if *v == version => (*s, *n),
        _ => {
            let since = r["step"]["sinceMs"].as_u64().filter(|t| *t > 0 && *t <= now).unwrap_or(now);
            *seen = Some((version, since, stale.max(1)));
            (since, stale.max(1))
        }
    };
    let done_here = at_start.saturating_sub(stale);
    let elapsed_s = now.saturating_sub(started) as f64 / 1e3;
    let per_copy_s = (done_here > 0).then(|| elapsed_s / done_here as f64).or(*app.svc.rollout_pace_s.lock().unwrap_or_else(|e| e.into_inner()));
    let phase = if r["held"].is_object() {
        "held".to_string()
    } else if let Some(s) = r["step"]["name"].as_str() {
        s.to_string()
    } else if r["verify"].is_object() {
        "verify".into()
    } else {
        "restarting".into()
    };
    Some(json!({"done": total.saturating_sub(stale), "total": total, "per_copy_s": per_copy_s.map(|s| (s * 10.0).round() / 10.0), "eta_s": per_copy_s.map(|s| (s * stale as f64).round()), "started_ms": started, "phase": phase}))
}

/// `GET /v1/admin/kube/versions?limit=30`: the images of the cluster's
/// repository (the StrataCluster's, else `--image-repository`) a person
/// may pick, newest first, each marked current, newer and tracked.
/// Without Kubernetes too, for the picker to be shown read-only.
async fn kube_versions(State(app): State<App>, Query(q): Params) -> Res {
    use crate::operator::upgrade::{self as up, Ref};
    let limit = param_u64(&q, "limit", 30)?.clamp(1, 500) as usize;
    let mut kube_error = None;
    let spec = match &app.svc.kube {
        Some(l) => match l.get().await {
            Ok(now) => Some(now["spec"].clone()),
            Err(e) => {
                kube_error = Some(format!("the Kubernetes API: {e}"));
                None
            }
        },
        None => None,
    };
    let image = spec.as_ref().and_then(|s| s["image"].as_str().map(String::from)).or_else(|| app.svc.place.image.clone());
    let repository = image.as_deref().and_then(|i| Ref::parse(i).ok()).map(|r| r.repository()).filter(|_| spec.is_some()).unwrap_or_else(|| app.svc.image_repository.clone());
    let upgrade: up::Upgrade = spec.as_ref().and_then(|s| serde_json::from_value(s["upgrade"].clone()).ok()).unwrap_or_default();
    let tracked = upgrade.tracked(image.as_deref().unwrap_or(&format!("{repository}:latest")));
    let track = Ref::parse(&tracked).ok().and_then(|r| r.tag).unwrap_or_else(|| "latest".into());
    // What runs now: the spec's digest, else the one most copies run, else
    // this server's own.
    let mut digest = image.as_deref().and_then(up::digest_of);
    if digest.is_none() {
        if let crate::service::Backend::Cluster(c) = &app.svc.backend {
            digest = copy_versions(&*c.snapshot().await).first().and_then(|v| v["digest"].as_str().map(String::from));
        }
    }
    let digest = digest.or_else(|| app.svc.place.digest.clone());
    let current = json!({"image": image, "digest": digest});
    let mut j = match up::Versions::get().list(&up::Registry::new(), &repository).await {
        Ok((offered, error)) => up::versions_json(&repository, &track, &current, &offered, error, limit),
        Err(e) => json!({"repository": repository, "track": track, "current": current, "images": [], "error": format!("the registry: {e}")}),
    };
    j["managed"] = json!(app.svc.kube.is_some());
    if let Some(e) = kube_error {
        j["kube_error"] = json!(e);
    }
    Ok(AxJson(j))
}

/// The spec patch a `POST /v1/admin/kube` or `/v1/admin/kube/plan` body
/// asks for of the `StrataCluster` `now`: `{"spec": {...}}` (any of the
/// spec's fields), `{"rollback": version}` (the spec of that version in
/// its `status.history`), or the sizes alone (`{"shards"?, "replicas"?,
/// "coordinators"?}`).
fn kube_patch(b: &Json, now: &Json) -> Result<Json, ApiError> {
    if let Some(v) = b.get("rollback") {
        let v = v.as_u64().ok_or_else(|| ApiError::bad("rollback is the number of a version in the cluster's history"))?;
        return crate::operator::kube::Link::rollback(now, v).map_err(ApiError::bad);
    }
    if let Some(spec) = b.get("spec") {
        return match spec {
            Json::Object(_) => Ok(spec.clone()),
            _ => Err(ApiError::bad("spec is an object: the StrataCluster's spec fields to change")),
        };
    }
    let n = |k: &str| match b.get(k) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_u64().map(|n| Some(n as usize)).ok_or_else(|| ApiError::bad(format!("{k} is a number"))),
    };
    crate::operator::kube::Link::sizes(n("shards")?, n("replicas")?, n("coordinators")?).map_err(ApiError::bad)
}

/// The patch body `b` asks for, the spec it makes, and the plan of the
/// change ([`crate::operator::plan`]).
async fn kube_planned(l: &crate::operator::kube::Link, b: &Json) -> Result<(Json, crate::operator::Spec, Json), ApiError> {
    let now = l.get().await.map_err(|e| ApiError { code: None, status: 502, message: format!("the Kubernetes API: {e}") })?;
    let patch = kube_patch(b, &now)?;
    let (old, new) = crate::operator::patched(&now["spec"], &patch).map_err(ApiError::bad)?;
    let mut plan = crate::operator::plan(&old, &new, old.shards, old.replicas);
    // cluster2: the sizes before and after, which the estimate is of.
    plan["sizes"] = json!({"shards": [old.shards, new.shards], "replicas": [old.replicas, new.replicas]});
    if let Some(v) = b.get("rollback") {
        plan["rollback"] = v.clone();
        plan["patch"] = patch.clone();
        if let Some(w) = plan["warnings"].as_array_mut() {
            w.push(json!("a rollback: no gate holds it, so that it goes through while the version it leaves is unwell"));
        }
    }
    Ok((patch, new, plan))
}

fn kube_link(app: &App) -> Result<&crate::operator::kube::Link, ApiError> {
    app.svc.kube.as_ref().ok_or_else(|| ApiError::bad("this coordinator is not run by the Kubernetes operator: change the cluster with /v1/admin/shards"))
}

/// `POST /v1/admin/kube/plan {"spec": {...}}` (or `{"rollback": version}`):
/// what changing the `StrataCluster`'s spec so would change, the
/// operator's steps for it and what to know first; nothing is changed.
async fn kube_plan(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    let l = kube_link(&app)?;
    let (_, _, mut plan) = kube_planned(l, &b).await?;
    // cluster2: how long it takes and the rows it copies, said once here
    // for every view (NEW-cluster-admin-7).
    if let Ok(c) = app.svc.coordinator() {
        let n = |k: &str, i: usize| plan["sizes"][k][i].as_u64().unwrap_or(0) as usize;
        let (from, to) = ((n("shards", 0), n("replicas", 0)), (n("shards", 1), n("replicas", 1)));
        plan["estimate"] = crate::coord::plan::shape_estimate(&*c.snapshot().await, from, to, c.measured_rate());
    }
    Ok(AxJson(plan))
}

/// `POST /v1/admin/kube {"spec": {...}}` (or the sizes alone, or
/// `{"rollback": version}`): changes the `StrataCluster`'s spec, checked
/// as the operator would, and notes who did; the operator then makes the
/// change. A rollback is held by no gate.
async fn kube_change(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let l = kube_link(&app)?;
    let (patch, new, plan) = kube_planned(l, &b).await?;
    if let Some(why) = plan["refused"].as_str() {
        return Err(ApiError::bad(why.to_string()));
    }
    use crate::operator::kube::{CHANGED_BY, ROLLBACK};
    let mut notes = vec![(CHANGED_BY, format!("{} {}", who(w).name, new.version()))];
    let back = b.get("rollback").and_then(Json::as_u64);
    if back.is_some() {
        notes.push((ROLLBACK, new.version()));
    }
    let j = l.change(&patch, &notes).await.map_err(|e| ApiError { code: None, status: 502, message: format!("the Kubernetes API: {e}") })?;
    let said: Vec<String> = plan["changes"].as_array().into_iter().flatten().map(|c| format!("{} {} -> {}", c["field"].as_str().unwrap_or("?"), c["from"], c["to"])).collect();
    let said = if said.is_empty() { "nothing".into() } else { said.join(", ") };
    Ok(audited(j, match back {
        Some(v) => format!("rolled the StrataCluster {}/{} back to version {v}: {said}", l.namespace, l.name),
        None => format!("changed the StrataCluster {}/{}: {said}", l.namespace, l.name),
    }))
}

/// `POST /v1/admin/kube/upgrade {"image"?, "plan"?, "force"?}`: rolls the
/// cluster out to `image`, or to the newest image of the tag the upgrader
/// follows (`status.upgrade.latest`, pinned by its digest), as a change
/// of `spec.image` would: the image is tried on every node first, each
/// server restarts in turn while the others serve, and the version is
/// verified, then rolled back if it fails (`spec.rollout.onFailure`).
/// With `plan`, nothing is changed: the plan is answered. An image the
/// operator rolled back once is refused unless `force`.
async fn kube_upgrade(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let l = kube_link(&app)?;
    let now = l.get().await.map_err(|e| ApiError { code: None, status: 502, message: format!("the Kubernetes API: {e}") })?;
    let up = &now["status"]["upgrade"];
    let image = match b.get("image") {
        Some(Json::String(i)) if !i.trim().is_empty() => i.trim().to_string(),
        Some(Json::Null) | None => up["latest"]["image"].as_str().map(str::to_string).ok_or_else(|| {
            ApiError::bad(match up["error"].as_str() {
                Some(e) => format!("the registry could not be asked what the tag names ({e}): name the image"),
                None => "no newer image is known: name the image, or set spec.upgrade.mode to suggest so that the operator asks the registry what its tag names".into(),
            })
        })?,
        Some(_) => return Err(ApiError::bad("image is an image reference: ghcr.io/chloetech-ai/chloedb-server:TAG or …@sha256:DIGEST")),
    };
    crate::operator::upgrade::Ref::parse(&image).map_err(|e| ApiError::bad(format!("image {image}: {e}")))?;
    if now["spec"]["image"].as_str() == Some(image.as_str()) {
        return Err(ApiError::bad(format!("the cluster's spec names {image} already: nothing to roll out")));
    }
    let digest = crate::operator::upgrade::digest_of(&image);
    let refused = up["rejected"].as_array().into_iter().flatten().find(|r| digest.is_some() && r["digest"].as_str() == digest.as_deref());
    if let (Some(r), false) = (refused, b["force"].as_bool().unwrap_or(false)) {
        return Err(ApiError::bad(format!("{image} was rolled back once: {}; pass force to try it again", r["why"].as_str().unwrap_or("it failed a check"))));
    }
    let ask = json!({"spec": {"image": image}});
    let (patch, new, mut plan) = kube_planned(l, &ask).await?;
    let from = now["spec"]["image"].as_str().unwrap_or_default().to_string();
    let latest = &up["latest"];
    if latest["image"].as_str() == Some(image.as_str()) {
        plan["image"] = latest.clone();
    }
    plan["from"] = json!(from);
    upgrade_estimates(&app, &now, &image, &mut plan).await;
    if b["plan"].as_bool().unwrap_or(false) {
        return Ok(AxJson(plan).into_response());
    }
    if let Some(why) = plan["refused"].as_str() {
        return Err(ApiError::bad(why.to_string()));
    }
    use crate::operator::kube::CHANGED_BY;
    let j = l.change(&patch, &[(CHANGED_BY, format!("{} {}", who(w).name, new.version()))]).await.map_err(|e| ApiError { code: None, status: 502, message: format!("the Kubernetes API: {e}") })?;
    Ok(audited(j, format!("upgraded the StrataCluster {}/{} from {from} to {image}", l.namespace, l.name)))
}

/// What an upgrade to `image` restarts and takes: the copies whose image
/// differs (`restarts`, named in `restart_copies`), and estimates of the
/// rollout and of a rollback, in seconds: the last rollout's pace a copy
/// when the coordinator saw one, else 30 s a copy to restart and catch
/// up, with the spec's soak, the pre-flight and the verification.
async fn upgrade_estimates(app: &App, now: &Json, image: &str, plan: &mut Json) {
    let digest = crate::operator::upgrade::digest_of(image);
    let mut differ = Vec::new();
    if let crate::service::Backend::Cluster(c) = &app.svc.backend {
        let snap = c.snapshot().await;
        for r in snap["shards"].as_array().into_iter().flatten().flat_map(|s| s["replicas"].as_array().into_iter().flatten()) {
            let v = &r["version"];
            let same = match &digest {
                Some(d) => v["digest"].as_str() == Some(d.as_str()),
                None => v["image"].as_str() == Some(image),
            };
            if !same {
                differ.push(r["short"].clone());
            }
        }
    }
    let rollout: crate::operator::RolloutSpec = serde_json::from_value(now["spec"]["rollout"].clone()).unwrap_or_default();
    let coordinators = now["spec"]["coordinators"].as_u64().unwrap_or(1) as f64;
    let pace = *app.svc.rollout_pace_s.lock().unwrap_or_else(|e| e.into_inner());
    let per_copy = pace.unwrap_or(30.0 + rollout.soak_seconds as f64);
    let n = differ.len() as f64;
    let (preflight, coordinator_s) = (60.0, 20.0);
    plan["restarts"] = json!(differ.len());
    plan["restart_copies"] = json!(differ);
    plan["estimate_s"] = json!((preflight + n * per_copy + coordinators * coordinator_s + rollout.verify_seconds as f64).round());
    plan["rollback_estimate_s"] = json!((n * per_copy + coordinators * coordinator_s).round());
    plan["estimate_from"] = json!(match pace {
        Some(p) => format!("the last rollout's pace, {p:.0} s a copy"),
        None => format!("about {per_copy:.0} s a copy to restart and catch up (with spec.rollout.soakSeconds), 60 s of pre-flight, 20 s a coordinator and spec.rollout.verifySeconds"),
    });
}

/// `POST /v1/admin/kube/resume`: lifts the hold a gate put on the
/// operator's change; the gates count again from now.
async fn kube_resume(State(app): State<App>) -> Result<Response, ApiError> {
    let l = kube_link(&app)?;
    let held = l.get().await.map_err(|e| ApiError { code: None, status: 502, message: format!("the Kubernetes API: {e}") })?["status"]["rollout"]["held"]["text"].as_str().unwrap_or_default().to_string();
    let j = l.resume().await.map_err(ApiError::bad)?;
    Ok(audited(j, format!("resumed the change of the StrataCluster {}/{}, held as {held}", l.namespace, l.name)))
}

/// `POST /v1/admin/kube/autoscale/dismiss`: the autoscaler's suggestion
/// is not made; its rules wait their cooldown before they suggest again.
async fn autoscale_dismiss(State(app): State<App>) -> Result<Response, ApiError> {
    let l = kube_link(&app)?;
    let now = l.get().await.map_err(|e| ApiError { code: None, status: 502, message: format!("the Kubernetes API: {e}") })?;
    let text = now["status"]["autoscale"]["suggestion"]["text"].as_str().unwrap_or_default().to_string();
    let j = l.dismiss().await.map_err(ApiError::bad)?;
    Ok(audited(j, format!("dismissed the autoscaler's suggestion for the StrataCluster {}/{}: {text}", l.namespace, l.name)))
}

/// The steps of load the servers kept since `since_ms` (each log store's
/// `load`, a row a server a minute), ten minutes each: the busiest
/// shard's copies, how busy and how many, and the fullest volume. And
/// the servers that answered.
async fn load_steps(app: &App, since_ms: u64) -> Result<(Vec<crate::operator::autoscale::Step>, Json), ApiError> {
    let sql = "SELECT step, shard, SUM(busy), COUNT(*), MIN(minute), MAX(minute), MAX(disk_used_ratio) FROM load \
               WHERE ts > :since AND role IN ('primary', 'replica', 'standalone') GROUP BY step, shard LIMIT 10000";
    let params = chloedb::query::Params::new().set("since", chloedb::value::Value::Timestamp(since_ms as i64 * 1000));
    let timeout = app.svc.timeout_of(0);
    let store = app.svc.log_store.as_ref();
    let j = match &app.svc.backend {
        crate::service::Backend::Cluster(c) => crate::logstore::cluster_query(store, c, sql, &params, timeout, async { Vec::new() }).await?,
        _ => {
            let store = store.ok_or_else(crate::logstore::none_here)?;
            let (rs, ms) = store.query(sql, &params, false, timeout).await?;
            let mut j = js::result(&rs, ms);
            j["nodes"] = json!([{"node": store.options().node, "rows": rs.rows.len(), "ms": ms}]);
            j
        }
    };
    // By step: (busy, copies) of each shard, and the fullest volume.
    let mut by: std::collections::BTreeMap<u64, (f64, f64, Option<f64>)> = Default::default();
    for r in j["rows"].as_array().into_iter().flatten() {
        let (Some(step), Some(sum), Some(n)) = (r[0].as_u64(), r[2].as_f64(), r[3].as_f64()) else { continue };
        let minutes = (r[5].as_f64().unwrap_or(0.0) - r[4].as_f64().unwrap_or(0.0) + 1.0).max(1.0);
        let (busy, copies) = (sum / n.max(1.0), n / minutes);
        let e = by.entry(step).or_insert((f64::MIN, 0.0, None));
        if busy > e.0 {
            (e.0, e.1) = (busy, copies);
        }
        if let Some(d) = r[6].as_f64() {
            e.2 = Some(e.2.map_or(d, |x: f64| x.max(d)));
        }
    }
    let steps = by.into_iter().map(|(step, (busy, copies, full))| crate::operator::autoscale::Step { at_ms: step * 600_000, busy, copies, full }).collect();
    Ok((steps, j["nodes"].clone()))
}

/// `POST /v1/admin/autoscale/replay {"autoscale": rules, "days"?}`: the
/// rules ([`crate::operator::autoscale`]) on the load the servers kept of
/// the last `days` (7 at most, the log stores' retention): how busy the
/// copies would have been, how many there would have been, and each
/// change, beside the cluster at its size. Nothing is changed. The sizes
/// it starts from are the `StrataCluster`'s, or the cluster's as it is,
/// or `sizes` (`{replicas, shards, volume}`).
async fn autoscale_replay(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    use crate::operator::autoscale::{replay, Autoscale, Sizes};
    let rules: Autoscale = serde_json::from_value(b.get("autoscale").cloned().unwrap_or(json!({}))).map_err(|e| ApiError::bad(format!("autoscale: {e}")))?;
    rules.check().map_err(ApiError::bad)?;
    let days = b.get("days").and_then(Json::as_f64).unwrap_or(7.0);
    if !(days > 0.0 && days <= 7.0) {
        return Err(ApiError::bad(format!("days is more than 0 and at most 7 (the log stores keep a week), not {days}")));
    }
    let spec = match &app.svc.kube {
        Some(l) => l.get().await.ok().and_then(|c| serde_json::from_value::<crate::operator::Spec>(c["spec"].clone()).ok()),
        None => None,
    };
    let cluster = app.svc.cluster().await;
    let shards = cluster["shards"].as_array();
    let given = &b["sizes"];
    let n = |k: &str| given[k].as_u64().map(|v| v as usize).filter(|v| *v > 0);
    // cluster2 (NEW-cluster-admin-6): on machines of one's own, the volume
    // is the largest disk the copies report, so that the volumes rule
    // replays on the levels the copies' health gives.
    let disk = crate::coord::plan::volume_of(&cluster);
    let copies: Vec<usize> = shards.into_iter().flatten().filter_map(|s| s["replicas"].as_array().map(Vec::len)).collect();
    let start = Sizes {
        replicas: n("replicas").or(spec.as_ref().map(|s| s.replicas)).or_else(|| copies.iter().copied().max()).unwrap_or(1),
        shards: n("shards").or(spec.as_ref().map(|s| s.shards)).or_else(|| shards.map(Vec::len)).unwrap_or(1).max(1),
        volume: given["volume"].as_str().map(String::from).or(spec.as_ref().map(|s| s.storage.size.clone())).or(disk.clone()).unwrap_or_else(|| "10Gi".into()),
    };
    let now = crate::telemetry::now_ms();
    // A step a shard a row: at most 10,000 rows.
    let days = days.min(10_000.0 / (144.0 * start.shards as f64));
    let since = now.saturating_sub((days * 86_400_000.0) as u64);
    let (steps, nodes) = load_steps(&app, since).await?;
    // On machines of one's own no volume size is known (no StrataCluster,
    // none given): the rule growing volumes is left out of the replay.
    let volume_known = spec.is_some() || given["volume"].is_string() || disk.is_some();
    let mut replayed = rules.clone();
    let mut left_out = Vec::new();
    if !volume_known && replayed.volumes.as_ref().is_some_and(|v| v.action == crate::operator::autoscale::VolumeAction::Grow) {
        replayed.volumes = None;
        left_out.push("volumes: no volume size is known without a StrataCluster, so growing volumes is left out (name sizes.volume to replay it)");
    }
    let mut j = replay(&replayed, &steps, &start);
    j["rules"] = json!(rules.said());
    j["left_out"] = json!(left_out);
    j["sizes"] = json!({"replicas": start.replicas, "shards": start.shards, "volume": if volume_known { json!(start.volume) } else { Json::Null }});
    if !volume_known {
        j["summary"]["end"]["volume"] = Json::Null;
    }
    // The copies each shard has now: a replay models every shard at
    // `sizes.replicas`, which a view says when they differ.
    j["copies_now"] = json!({"min": copies.iter().min(), "max": copies.iter().max()});
    if spec.is_none() && !given["volume"].is_string() {
        j["volume_from"] = json!(if disk.is_some() { "the copies' disks" } else { "unknown" });
    }
    j["sinceMs"] = json!(since);
    j["days"] = json!(days);
    j["nodes"] = nodes;
    j["managed"] = json!(spec.is_some());
    Ok(AxJson(j))
}

/// An AWS failure: 400 without credentials or for a region that is no
/// region's name, else 502 (AWS's answer).
fn aws_error(e: String) -> ApiError {
    ApiError { code: None, status: if e.starts_with("no AWS credentials") || e.contains("is not a region") { 400 } else { 502 }, message: e }
}

/// The cloud view's AWS state: the credentials as far as they may be shown (the
/// key id masked; never the secret nor the session token).
async fn aws_status(State(app): State<App>) -> AxJson<Json> {
    AxJson(app.svc.aws.status())
}

/// `{"access_key_id", "secret_access_key", "session_token"?, "region"}`:
/// checked with AWS (STS), then kept on this server. Answers who they are.
async fn aws_save(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let c: crate::aws::Creds = serde_json::from_value(b).map_err(|e| ApiError::bad(format!("the body is {{access_key_id, secret_access_key, session_token?, region}}: {e}")))?;
    let key = crate::aws::mask(&c.access_key_id);
    let who = who(w);
    // Refused or malformed credentials are the caller's; failing to write them, ours.
    let i = app.svc.aws.set(c, &who.name).await.map_err(|e| ApiError { code: None, status: if e.starts_with("write ") { 500 } else { 400 }, message: e })?;
    let mut j = app.svc.aws.status();
    j["identity"] = json!(i);
    Ok(audited(j, format!("saved the AWS credentials {key} (account {}, {})", i.account, i.arn)))
}

async fn aws_forget(State(app): State<App>) -> Result<Response, ApiError> {
    let was = app.svc.aws.forget().map_err(ApiError::internal)?;
    let detail = match &was {
        Some(k) => format!("forgot the AWS credentials {k}"),
        None => "forgot the AWS credentials (none were saved)".into(),
    };
    Ok(audited(json!({"forgot": was, "status": app.svc.aws.status()}), detail))
}

async fn aws_regions(State(app): State<App>) -> Result<AxJson<Json>, ApiError> {
    Ok(AxJson(json!({"regions": app.svc.aws.regions().await.map_err(aws_error)?})))
}

/// `?region=` (default: the credentials' region), `&fresh=1` to fetch
/// again: the region's machines with their prices, and the account's
/// quotas there.
async fn aws_machines(State(app): State<App>, Query(q): Query<std::collections::HashMap<String, String>>) -> Result<AxJson<Json>, ApiError> {
    let region = match q.get("region").filter(|r| !r.is_empty()) {
        Some(r) => r.clone(),
        None => app.svc.aws.creds().map(|(c, _)| c.region).ok_or_else(|| aws_error("no AWS credentials: save an access key in the Cloud view (PUT /v1/admin/aws/credentials)".into()))?,
    };
    let fresh = q.get("fresh").is_some_and(|v| v != "0" && v != "false");
    let cat = app.svc.aws.catalog(&region, fresh).await.map_err(aws_error)?;
    let mut j = json!(&*cat);
    let recommended: Vec<&str> = crate::aws::catalog::recommended(&cat.machines).iter().map(|m| m.name.as_str()).collect();
    j["recommended"] = json!(recommended);
    Ok(AxJson(j))
}

async fn aws_planned(app: &App, b: Json) -> Result<(crate::aws::stack::Plan, Json), ApiError> {
    let p: crate::aws::stack::Plan = serde_json::from_value(b).map_err(|e| ApiError::bad(format!("a plan: {e}")))?;
    let cat = app.svc.aws.catalog(&p.region, false).await.map_err(aws_error)?;
    let who = app.svc.aws.principal().await.map_err(aws_error)?;
    let j = crate::aws::stack::plan(&p, &cat, &who).map_err(ApiError::bad)?;
    Ok((p, j))
}

/// A plan of a cluster on EKS ([`crate::aws::stack`]): where its copies
/// go, its cost, its warnings, its CloudFormation template and its
/// StrataCluster. Creates nothing.
async fn aws_plan(State(app): State<App>, AxJson(b): AxJson<Json>) -> Result<AxJson<Json>, ApiError> {
    Ok(AxJson(aws_planned(&app, b).await?.1))
}

/// The plan, and CloudFormation's check of its template (free; creates
/// nothing).
async fn aws_validate(State(app): State<App>, AxJson(b): AxJson<Json>) -> Result<AxJson<Json>, ApiError> {
    let (p, mut j) = aws_planned(&app, b).await?;
    j["validated"] = match app.svc.aws.validate(&p.region, &j["template"]).await {
        Ok(v) => json!({"ok": true, "answer": v}),
        Err(e) => json!({"ok": false, "error": e}),
    };
    Ok(AxJson(j))
}

/// The hub's book ([`crate::hub`]): who this hub is, where it keeps the
/// keys, and its connections (no key among them).
async fn hub_view(State(app): State<App>) -> AxJson<Json> {
    AxJson(app.svc.hub.view())
}

/// `POST /v1/admin/hub/connections {name, url, key, color?, trust?,
/// allow_plain?, replace?}`: adds a connection once its address answers
/// and takes the key. 409 with `needs_trust` and the certificate's
/// fingerprint for a self-signed certificate not yet trusted.
async fn hub_add(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let a: crate::hub::Add = serde_json::from_value(b).map_err(|e| ApiError::bad(format!("a connection is {{name, url, key, color?, trust?, allow_plain?}}: {e}")))?;
    let by = keys::audit_name(&who(w));
    match app.svc.hub.add(a, &by).await? {
        crate::hub::Added::Done(c) => {
            tracing::info!(connection = %c.name, urls = %c.urls.join(","), by = %by, "hub connection added");
            keys::note_change(keys::Change { object: Some(format!("connection {}", c.name)), after: Some(json!({"urls": c.urls, "pin": c.pin, "key": c.key})), ..Default::default() });
            Ok((StatusCode::CREATED, AxJson(c.view())).into_response())
        }
        crate::hub::Added::NeedsTrust { url, fingerprint } => Ok((
            StatusCode::CONFLICT,
            AxJson(json!({"error": format!("{url}'s certificate is not signed by an authority this machine knows: compare its SHA-256 with the cluster's own, then trust it"), "needs_trust": true, "fingerprint": fingerprint, "url": url})),
        )
            .into_response()),
    }
}

/// `POST /v1/admin/hub/connections/:name {color?, key?}`.
async fn hub_update(State(app): State<App>, axum::extract::Path(name): axum::extract::Path<String>, AxJson(b): AxJson<Json>) -> Result<AxJson<Json>, ApiError> {
    let k = hub_kube(&app, &name).await?;
    let c = app.svc.hub.update(&name, &b, k.as_ref()).await?;
    keys::note_change(keys::Change { object: Some(format!("connection {name}")), after: Some(json!({"color": b.get("color"), "key": b.get("key").map(|_| &c.key)})), ..Default::default() });
    Ok(AxJson(c.view()))
}

async fn hub_remove(State(app): State<App>, axum::extract::Path(name): axum::extract::Path<String>) -> Result<AxJson<Json>, ApiError> {
    let c = app.svc.hub.remove(&name)?;
    keys::note_change(keys::Change { object: Some(format!("connection {name}")), before: Some(json!({"urls": c.urls})), ..Default::default() });
    Ok(AxJson(json!({"removed": name, "note": "its key still works on the cluster until it expires or is revoked there"})))
}

async fn hub_check(State(app): State<App>, axum::extract::Path(name): axum::extract::Path<String>) -> Result<AxJson<Json>, ApiError> {
    let k = hub_kube(&app, &name).await?;
    Ok(AxJson(app.svc.hub.check(&name, k.as_ref()).await?.view()))
}

/// A client of the Kubernetes API a connection goes through, for one made
/// from the Cloud view ([`crate::hub::ViaKube`]); none for one reached at
/// its address.
async fn hub_kube(app: &App, name: &str) -> Result<Option<crate::operator::kube::Kube>, ApiError> {
    crate::copy::kube_for(&app.svc, name).await
}

/// `POST /v1/admin/hub/adopt {made, name?, color?}`: the cluster `made`
/// (made from the Cloud view) in the book, reached through its Kubernetes
/// API with a key of this hub's own ([`crate::hub::Hub::adopt`]); the
/// connection that has it already, checked again, when there is one.
async fn hub_adopt(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let id = b["made"].as_str().ok_or_else(|| ApiError::bad("made is the id of a cluster made from the Cloud view"))?;
    let by = keys::audit_name(&who(w));
    let (c, new) = crate::copy::adopt_made(&app.svc, id, b["name"].as_str().map(String::from), b["color"].as_str().map(String::from), &by).await?;
    if !new {
        return Ok(AxJson(c.view()).into_response());
    }
    let through = c.kube.as_ref().map(|v| v.cluster.clone());
    tracing::info!(connection = %c.name, cluster = ?through, by = %by, "hub connection added through the cluster's Kubernetes API");
    keys::note_change(keys::Change { object: Some(format!("connection {}", c.name)), after: Some(json!({"through": through, "key": c.key})), ..Default::default() });
    Ok((StatusCode::CREATED, AxJson(c.view())).into_response())
}

/// `POST /v1/admin/hub/copy {from, to, tables}`: copies the tables from
/// one end to the other ([`crate::copy`]): each end a connection's name,
/// or `""` for this server. The target's import answer.
async fn hub_copy(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let from = crate::copy::End::parse(&b["from"])?;
    let to = crate::copy::End::parse(&b["to"])?;
    let tables: Vec<String> = b["tables"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default();
    let who = who(w);
    let j = crate::copy::copy(&app.svc, &who, &from, &to, &tables).await?;
    let detail = format!("copied {} from {} to {}: {} rows", tables.join(", "), from.describe(), to.describe(), j["written"].as_u64().unwrap_or(0));
    Ok(audited(j, detail))
}

/// `POST /v1/admin/hub/follow {follower, source, name?, start?,
/// source_url?, key_secret?, ca_secret?, plan?}`: sets the connection
/// `follower` to follow `source` ([`crate::hub_follow`]): a feed key made
/// on the source, kept in a Secret of the follower's namespace, and its
/// `spec.follow`. With `plan`, what would be done, and why it would be
/// refused (`refused`); nothing is made.
async fn hub_follow(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let a = crate::hub_follow::Ask::from(&b)?;
    let j = crate::hub_follow::follow(&app.svc, &who(w), &a).await?;
    if a.plan {
        return Ok(AxJson(j).into_response());
    }
    let detail = format!("set {} to follow {} (as {}, at {})", a.follower, a.source, j["name"].as_str().unwrap_or_default(), j["source_url"].as_str().unwrap_or_default());
    Ok(audited(j, detail))
}

/// `/c/:name/<path>`: the call, to the connection `name` with its key
/// ([`crate::hub::Hub::forward`]). Strata's API and its health only.
async fn hub_forward(State(app): State<App>, axum::extract::Path((name, rest)): axum::extract::Path<(String, String)>, req: Request) -> Result<Response, ApiError> {
    let rest = rest.trim_start_matches('/');
    if !(rest.starts_with("v1/") || matches!(rest, "livez" | "readyz" | "metrics")) {
        return Err(ApiError { code: None, status: 404, message: format!("/c/{name}/ forwards ChloeDB's API (/v1/...), not /{rest}") });
    }
    // The path as decoded: no dot segment (once or twice encoded) to climb
    // out of /v1, and no ? or # to start a query (audit D-B9).
    if rest.split('/').any(crate::apps::dot_segment) || rest.contains(['?', '#']) {
        return Err(ApiError::bad("a path the hub forwards has no . or .. segment, ? or # in it"));
    }
    let path = format!("/{rest}{}", req.uri().query().map(|q| format!("?{q}")).unwrap_or_default());
    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes()).map_err(|_| ApiError::bad("an HTTP method the hub does not forward"))?;
    let headers = req.headers().clone();
    let body = reqwest::Body::wrap_stream(req.into_body().into_data_stream());
    let k = hub_kube(&app, &name).await?;
    app.svc.hub.forward(&name, method, &path, &headers, body, k.as_ref()).await
}

/// The clusters the cloud view made ([`crate::cloud::clusters`]), the
/// newest first, each with where it is, what it has cost so far and how
/// to reach it.
async fn cloud_list(State(app): State<App>) -> AxJson<Json> {
    let book = app.svc.hub.list();
    let list: Vec<Json> = app
        .svc
        .clouds
        .list()
        .iter()
        .map(|m| {
            let mut v = m.view();
            // The hub's connection to it, once its command center was opened.
            v["connection"] = json!(book.iter().find(|c| c.kube.as_ref().is_some_and(|k| k.made == m.id)).map(|c| &c.name));
            v
        })
        .collect();
    AxJson(json!({"clusters": list, "file": app.svc.clouds.file().map(|f| f.display().to_string())}))
}

async fn cloud_one(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> Result<AxJson<Json>, ApiError> {
    let m = app.svc.clouds.get(&id).ok_or_else(|| ApiError { code: None, status: 404, message: format!("no cluster {id}") })?;
    Ok(AxJson(m.view()))
}

/// `{"provider", "plan", "confirm", "delete_after_hours" | "keep", "bring"?}`:
/// makes the plan's cluster, which bills the account until it is deleted.
/// `confirm` is the cluster's name, typed again; `delete_after_hours` has
/// it delete itself then, and without it `keep: true` says it is kept
/// until someone deletes it; `bring` names tables of this server the
/// cluster copies in once it is ready ([`crate::copy::bring_when_ready`]).
/// Refused when this server would forget the cluster or the account's key
/// when it restarts (finding E-6: a coordinator keeps no file by default,
/// and the cluster then bills until someone finds it in the cloud's
/// console), when the account's limit would not start the nodes, or when
/// the cloud already has a cluster of that name. 202: the cluster is on
/// its way; `GET` it to follow.
async fn cloud_create(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    use crate::cloud::clusters::{Made, Phase};
    let provider = b["provider"].as_str().unwrap_or_default().to_string();
    let plan = b["plan"].clone();
    let refuse = |m: String| ApiError { code: None, status: 409, message: m };
    cloud_durable(&app, &provider).map_err(refuse)?;
    let (j, nodes) = match provider.as_str() {
        "aws" => {
            let (p, j) = aws_planned(&app, plan).await?;
            let (needs, may) = (j["quota"]["needs"].as_f64().unwrap_or(0.0), j["quota"]["vcpus"].as_f64());
            if may.is_some_and(|q| needs > q) {
                return Err(refuse(format!("the nodes need {needs} vCPUs of {} instances and the account may run {} in {}: ask AWS for more first (Service Quotas)", j["quota"]["kind"].as_str().unwrap_or("standard"), may.unwrap_or(0.0), p.region)));
            }
            if let Some(s) = app.svc.aws.stack(&p.region, j["stack"].as_str().unwrap_or_default()).await.map_err(aws_error)? {
                if s.status != "DELETE_COMPLETE" {
                    return Err(refuse(format!("{} already has a stack named {} ({}): pick another name", p.region, j["stack"].as_str().unwrap_or_default(), s.status)));
                }
            }
            (j, p.nodes)
        }
        "digitalocean" => {
            let p: crate::digitalocean::plan::Plan = serde_json::from_value(plan).map_err(|e| ApiError::bad(format!("a plan: {e}")))?;
            let cat = app.svc.digitalocean.catalog(&p.region, true).await.map_err(do_error)?;
            let j = crate::digitalocean::plan::plan(&p, &cat).map_err(ApiError::bad)?;
            let q = &j["quota"];
            if let (Some(limit), Some(running)) = (q["droplets"].as_u64(), q["running"].as_u64()) {
                if running + u64::from(p.nodes) > limit {
                    return Err(refuse(format!("the nodes need {} droplets and the account runs {running} of its {limit}: ask DigitalOcean for more first", p.nodes)));
                }
            }
            if app.svc.digitalocean.cluster_named(j["cluster_name"].as_str().unwrap_or_default()).await.map_err(do_error)?.is_some() {
                return Err(refuse(format!("the account already has a cluster named {}: pick another name", j["cluster_name"].as_str().unwrap_or_default())));
            }
            (j, p.nodes)
        }
        _ => return Err(ApiError::bad("provider is aws or digitalocean")),
    };
    let cluster = j["cluster_name"].as_str().unwrap_or_default().to_string();
    let after_ms = delete_after(&b["delete_after_hours"])?;
    let bring: Vec<String> = match &b["bring"] {
        Json::Null => Vec::new(),
        Json::Array(a) => a.iter().map(|t| t.as_str().map(String::from).ok_or_else(|| ApiError::bad("bring is a list of this server's tables"))).collect::<Result<_, _>>()?,
        _ => return Err(ApiError::bad("bring is a list of this server's tables")),
    };
    if !bring.is_empty() {
        if !matches!(app.svc.backend, crate::service::Backend::Local(_)) {
            return Err(ApiError::bad("bring copies tables of this server, and a coordinator holds no rows"));
        }
        let here = app.svc.tables_listed().await;
        if let Some(t) = bring.iter().find(|t| !here.iter().any(|s| s.table.eq_ignore_ascii_case(t))) {
            return Err(ApiError::bad(format!("this server has no table {t} to bring")));
        }
    }
    if b["confirm"].as_str() != Some(cluster.as_str()) {
        return Err(ApiError::bad(format!("confirm is the cluster's name, {cluster}, typed again: making it bills the account until it is deleted")));
    }
    match (after_ms, b["keep"].as_bool()) {
        (Some(_), Some(true)) => return Err(ApiError::bad("delete_after_hours or keep, not both")),
        (None, Some(true)) | (Some(_), _) => {}
        (None, _) => return Err(ApiError::bad("delete_after_hours (it deletes itself then), or keep: true (it bills the account until someone deletes it)")),
    }
    let who = who(w);
    let now = crate::operator::now_ms();
    // Who asked for it, as a DigitalOcean tag (letters, digits, ':', '-',
    // '_') its console and API show. When it deletes itself is not a tag
    // (see the stack's tags, crate::cloud::clusters).
    let mut j = j;
    if provider == "digitalocean" {
        let tag = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_') { c } else { '-' }).take(200).collect::<String>();
        if let Some(tags) = j["request"]["tags"].as_array_mut() {
            tags.push(json!(format!("strata-owner:{}", tag(&keys::audit_name(&who)))));
        }
    }
    let m = Made {
        id: format!("{provider}-{cluster}-{now}"),
        provider: provider.clone(),
        name: j["cluster"]["name"].as_str().map(str::to_string).unwrap_or_else(|| cluster.trim_start_matches("strata-").to_string()),
        cluster: cluster.clone(),
        region: j["region"].as_str().unwrap_or_default().to_string(),
        machine: j["machine"]["type"].as_str().or(j["machine"]["name"].as_str()).unwrap_or_default().to_string(),
        nodes,
        cost_hour: j["cost"]["hour"].as_f64().unwrap_or(0.0),
        by: who.name.clone(),
        created_ms: now,
        phase: Phase::Creating,
        cloud_id: None,
        request: if provider == "aws" { j["template"].clone() } else { j["request"].clone() },
        image: j["cluster"]["image"].as_str().unwrap_or(crate::cloud::IMAGE).to_string(),
        objects: j["objects"].as_array().cloned().unwrap_or_default(),
        steps: Vec::new(),
        error: None,
        strata: Json::Null,
        progress: None,
        ready_ms: None,
        ended_ms: None,
        generation: 0,
        tries: 0,
        checked_ms: 0,
        deleting_since_ms: None,
        volumes_gone: false,
        delete_asked: false,
        delete_at_ms: after_ms.map(|a| now + a),
        bring,
        brought: None,
        found: false,
    };
    let m = app.svc.clouds.add(m).map_err(refuse)?;
    app.svc.clouds.start(app.svc.aws.clone(), app.svc.digitalocean.clone());
    crate::copy::bring_when_ready(app.svc.clone(), m.id.clone());
    let what = if provider == "aws" { "AWS" } else { "DigitalOcean" };
    let detail = format!("asked {what} for the cluster {cluster} in {}: {} × {}, ${:.3} an hour", m.region, m.nodes, m.machine, m.cost_hour);
    let mut r = audited(m.view(), detail);
    *r.status_mut() = axum::http::StatusCode::ACCEPTED;
    Ok(r)
}

/// Whether a cluster made here, and the key of `provider`'s account that
/// deletes it, outlive a restart of this server: kept in files, or the key
/// in the environment. A coordinator keeps no file unless it is given one
/// (`--cloud-file`, `--aws-file`, `--digitalocean-file`): its list of made
/// clusters, their deletion times and the key would last as long as the
/// process (finding E-6).
fn cloud_durable(app: &App, provider: &str) -> Result<(), String> {
    let svc = &app.svc;
    if svc.clouds.file().is_none() {
        return Err("this server keeps the clusters it makes in memory only (a coordinator keeps no file unless given one): restarted, it would forget this one, which would bill the account until someone deletes it in the cloud's console, and would not delete itself when asked to. Start it with --cloud-file on a disk that outlives it, or make the cluster from a server of a database".into());
    }
    let memory_key = match provider {
        "aws" => svc.aws.creds().is_some_and(|(_, source)| source == "stored") && svc.aws.file().is_none(),
        "digitalocean" => svc.digitalocean.token().is_some_and(|(_, source)| source == "stored") && svc.digitalocean.file().is_none(),
        _ => false,
    };
    if memory_key {
        let flag = if provider == "aws" { "--aws-file" } else { "--digitalocean-file" };
        return Err(format!("this server keeps the account's key in memory only: restarted, it could not delete the cluster. Start it with {flag} on a disk that outlives it, or give the key in its environment"));
    }
    Ok(())
}

/// `delete_after_hours`: a number of hours (above 0, at most 30 days) as
/// milliseconds, or none.
fn delete_after(v: &Json) -> Result<Option<u64>, ApiError> {
    match v {
        Json::Null => Ok(None),
        v => {
            let h = v.as_f64().filter(|h| *h > 0.0 && *h <= 720.0).ok_or_else(|| ApiError::bad("delete_after_hours is a number of hours above 0, at most 720 (30 days), or null to keep it"))?;
            Ok(Some((h * 3_600_000.0) as u64))
        }
    }
}

/// `{"hours"}`: the cluster deletes itself that many hours from now;
/// `null` keeps it until someone deletes it.
async fn cloud_expiry(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let after = delete_after(&b["hours"])?;
    let by = who(w).name.clone();
    let at = after.map(|a| crate::operator::now_ms() + a);
    let m = app.svc.clouds.delete_at(&id, at, &by).map_err(|e| ApiError { code: None, status: 409, message: e })?;
    let detail = match at {
        Some(t) => format!("set the cluster {} to delete itself at {} UTC", m.cluster, crate::cloud::clusters::utc(t)),
        None => format!("kept the cluster {}: it no longer deletes itself", m.cluster),
    };
    Ok(audited(m.view(), detail))
}

/// `{"confirm"}`: deletes a cluster made here: the StrataCluster and its
/// volumes, then the cloud's cluster. `confirm` is its name.
async fn cloud_delete(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let m = app.svc.clouds.get(&id).ok_or_else(|| ApiError { code: None, status: 404, message: format!("no cluster {id}") })?;
    if b["confirm"].as_str() != Some(m.cluster.as_str()) {
        return Err(ApiError::bad(format!("confirm is the cluster's name, {}, typed again: deleting it deletes its data", m.cluster)));
    }
    let who = who(w);
    let m = app.svc.clouds.delete(&id, &who.name).map_err(|e| ApiError { code: None, status: 409, message: e })?;
    app.svc.clouds.start(app.svc.aws.clone(), app.svc.digitalocean.clone());
    let detail = format!("deleting the cluster {} on {} in {}", m.cluster, if m.provider == "aws" { "AWS" } else { "DigitalOcean" }, m.region);
    let mut r = audited(m.view(), detail);
    *r.status_mut() = axum::http::StatusCode::ACCEPTED;
    Ok(r)
}

/// The ChloeDB clusters the cloud's account holds that this server's list
/// does not follow (made from another server, or forgotten here):
/// `?provider=digitalocean` (every region), or `?provider=aws&region=`
/// (CloudFormation's stacks are a region's). Each says whether this
/// server runs on it (`here`), which may not be deleted from here.
async fn cloud_found(State(app): State<App>, Query(q): Query<std::collections::HashMap<String, String>>) -> Result<AxJson<Json>, ApiError> {
    Ok(AxJson(json!({"clusters": found(&app, q.get("provider").map(String::as_str).unwrap_or_default(), q.get("region").map(String::as_str)).await?})))
}

async fn found(app: &App, provider: &str, region: Option<&str>) -> Result<Vec<Json>, ApiError> {
    let made = app.svc.clouds.list();
    match provider {
        "digitalocean" => {
            let list = app.svc.digitalocean.clusters().await.map_err(do_error)?;
            Ok(crate::cloud::clusters::found_doks(&list, &made, app.svc.place.node.as_deref()))
        }
        "aws" => {
            let region = region.filter(|r| !r.is_empty()).ok_or_else(|| ApiError::bad("region: CloudFormation's stacks are a region's"))?;
            let stacks = app.svc.aws.stacks(region).await.map_err(aws_error)?;
            Ok(crate::cloud::clusters::found_stacks(region, &stacks, &made))
        }
        _ => Err(ApiError::bad("provider is aws or digitalocean")),
    }
}

/// `{"provider", "cloud_id", "region"?, "confirm"}`: deletes a cluster
/// [`cloud_found`] lists, as one made here is deleted (its volumes, then
/// the cloud's cluster), after it is put on this server's list to be
/// followed. `confirm` is its name, typed again. Refused for one this
/// list has already, one the cloud no longer has, and the one this server
/// runs on (deleting it would stop this server halfway).
async fn cloud_found_delete(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    use crate::cloud::clusters::{Made, Phase};
    let provider = b["provider"].as_str().unwrap_or_default().to_string();
    let id = b["cloud_id"].as_str().filter(|i| !i.is_empty()).ok_or_else(|| ApiError::bad("cloud_id is the cloud's id of the cluster, as /v1/admin/cloud/found lists it"))?;
    let c = found(&app, &provider, b["region"].as_str()).await?.into_iter().find(|c| c["cloud_id"] == id).ok_or_else(|| ApiError {
        code: None, status: 404,
        message: format!("the account has no ChloeDB cluster {id} that this server does not follow already"),
    })?;
    let cluster = c["cluster"].as_str().unwrap_or_default().to_string();
    if c["here"] == true {
        return Err(ApiError { code: None, status: 409, message: format!("this server runs on {cluster}: deleting it from here would stop it halfway; delete it from another server, or from the cloud's console") });
    }
    if b["confirm"].as_str() != Some(cluster.as_str()) {
        return Err(ApiError::bad(format!("confirm is the cluster's name, {cluster}, typed again: deleting it deletes its data")));
    }
    let who = who(w);
    let now = crate::operator::now_ms();
    let m = Made {
        id: format!("{provider}-{cluster}-{now}"),
        provider: provider.clone(),
        name: cluster.trim_start_matches("strata-").to_string(),
        cluster: cluster.clone(),
        region: c["region"].as_str().unwrap_or_default().to_string(),
        machine: c["machine"].as_str().unwrap_or_default().to_string(),
        nodes: c["nodes"].as_u64().unwrap_or(0) as u32,
        cost_hour: 0.0,
        by: who.name.clone(),
        created_ms: now,
        phase: Phase::Deleting,
        cloud_id: Some(id.to_string()),
        request: Json::Null,
        image: String::new(),
        objects: Vec::new(),
        steps: Vec::new(),
        error: None,
        strata: Json::Null,
        progress: None,
        ready_ms: None,
        ended_ms: None,
        generation: 0,
        tries: 0,
        checked_ms: 0,
        deleting_since_ms: None,
        volumes_gone: false,
        delete_asked: false,
        delete_at_ms: None,
        bring: Vec::new(),
        brought: None,
        found: true,
    };
    let m = app.svc.clouds.adopt(m, &who.name).map_err(|e| ApiError { code: None, status: 409, message: e })?;
    app.svc.clouds.start(app.svc.aws.clone(), app.svc.digitalocean.clone());
    let detail = format!("deleting the cluster {} on {} in {}, found in the account", m.cluster, if m.provider == "aws" { "AWS" } else { "DigitalOcean" }, m.region);
    let mut r = audited(m.view(), detail);
    *r.status_mut() = axum::http::StatusCode::ACCEPTED;
    Ok(r)
}

/// Forgets a cluster that is deleted, or that failed before the cloud
/// made anything; 409 while it may still bill.
async fn cloud_forget(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> Result<Response, ApiError> {
    let m = app.svc.clouds.forget(&id).map_err(|e| ApiError { code: None, status: if e.starts_with("no cluster") { 404 } else { 409 }, message: e })?;
    Ok(audited(json!({"forgot": m.view()}), format!("forgot the cluster {} ({:?})", m.cluster, m.phase)))
}

/// A DigitalOcean failure: 400 without a token or for a region that is no
/// region's name, else 502 (DigitalOcean's answer).
fn do_error(e: String) -> ApiError {
    ApiError { code: None, status: if e.starts_with("no DigitalOcean token") || e.contains("is not a region") || e.starts_with("DOKS does not run") { 400 } else { 502 }, message: e }
}

/// The cloud view's DigitalOcean state: the token as far as it may be
/// shown (masked).
async fn do_status(State(app): State<App>) -> AxJson<Json> {
    AxJson(app.svc.digitalocean.status())
}

/// `{"token"}`: checked with DigitalOcean (`GET /v2/account`), then kept on
/// this server. Answers whose it is.
async fn do_save(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let token = b["token"].as_str().ok_or_else(|| ApiError::bad("the body is {\"token\": \"dop_v1_...\"}"))?.to_string();
    let masked = crate::aws::mask(token.trim());
    let who = who(w);
    let a = app.svc.digitalocean.set(token, &who.name).await.map_err(|e| ApiError { code: None, status: if e.starts_with("write ") { 500 } else { 400 }, message: e })?;
    let mut j = app.svc.digitalocean.status();
    j["account"] = json!(a);
    Ok(audited(j, format!("saved the DigitalOcean token {masked} (team {}, account {})", a.team, a.uuid)))
}

async fn do_forget(State(app): State<App>) -> Result<Response, ApiError> {
    let was = app.svc.digitalocean.forget().map_err(ApiError::internal)?;
    let detail = match &was {
        Some(k) => format!("forgot the DigitalOcean token {k}"),
        None => "forgot the DigitalOcean token (none was saved)".into(),
    };
    Ok(audited(json!({"forgot": was, "status": app.svc.digitalocean.status()}), detail))
}

async fn do_regions(State(app): State<App>) -> Result<AxJson<Json>, ApiError> {
    Ok(AxJson(json!({"regions": app.svc.digitalocean.regions().await.map_err(do_error)?})))
}

/// `?region=` (default nyc3), `&fresh=1` to fetch again: the sizes a DOKS
/// node may be there with their prices, DOKS's versions and the account's
/// droplets.
async fn do_machines(State(app): State<App>, Query(q): Query<std::collections::HashMap<String, String>>) -> Result<AxJson<Json>, ApiError> {
    let region = q.get("region").filter(|r| !r.is_empty()).cloned().unwrap_or_else(|| "nyc3".into());
    let fresh = q.get("fresh").is_some_and(|v| v != "0" && v != "false");
    let cat = app.svc.digitalocean.catalog(&region, fresh).await.map_err(do_error)?;
    let mut j = json!(&*cat);
    let recommended: Vec<&str> = crate::digitalocean::recommended(&cat.machines).iter().map(|m| m.name.as_str()).collect();
    j["recommended"] = json!(recommended);
    Ok(AxJson(j))
}

/// A plan of a cluster on DOKS ([`crate::digitalocean::plan`]). Creates
/// nothing.
async fn do_plan(State(app): State<App>, AxJson(b): AxJson<Json>) -> Result<AxJson<Json>, ApiError> {
    let p: crate::digitalocean::plan::Plan = serde_json::from_value(b).map_err(|e| ApiError::bad(format!("a plan: {e}")))?;
    let cat = app.svc.digitalocean.catalog(&p.region, false).await.map_err(do_error)?;
    Ok(AxJson(crate::digitalocean::plan::plan(&p, &cat).map_err(ApiError::bad)?))
}

/// The plan, and DigitalOcean's answers now: the account, the region,
/// the size, the version and the droplet limit, fetched again. Creates
/// nothing.
async fn do_validate(State(app): State<App>, AxJson(b): AxJson<Json>) -> Result<AxJson<Json>, ApiError> {
    let p: crate::digitalocean::plan::Plan = serde_json::from_value(b).map_err(|e| ApiError::bad(format!("a plan: {e}")))?;
    let d = &app.svc.digitalocean;
    let cat = d.catalog(&p.region, true).await.map_err(do_error)?;
    let mut j = crate::digitalocean::plan::plan(&p, &cat).map_err(ApiError::bad)?;
    let token = d.token().map(|(t, _)| t).ok_or_else(|| do_error("no DigitalOcean token".into()))?;
    j["validated"] = match d.account(&token).await {
        Ok(a) => {
            let checks = crate::digitalocean::plan::checks(&p, &cat, &a.status);
            json!({"ok": checks.iter().all(|c| c["ok"] == true), "checks": checks})
        }
        Err(e) => json!({"ok": false, "error": e}),
    };
    Ok(AxJson(j))
}

/// `GET /v1/routes`: requests by route and queries by kind since the
/// server started, or with `from` (and `to`, ms) over that window.
async fn routes(State(app): State<App>, Query(q): Params) -> Res {
    if !q.contains_key("from") {
        return Ok(AxJson(app.svc.metrics.routes()));
    }
    let now = telemetry::now_ms();
    let (from, to) = (param_u64(&q, "from", 0)?, param_u64(&q, "to", now)?);
    if to < from {
        return Err(ApiError::bad("to comes before from"));
    }
    Ok(AxJson(app.svc.metrics.routes_between(from, to)))
}

async fn whoami(State(app): State<App>, w: Option<axum::Extension<Principal>>) -> AxJson<Json> {
    let w = who(w);
    let mut j = w.json();
    // `app_keys`: this server makes keys limited to tables, and sends each
    // app its own token ([`crate::apps`]), as the operator asks before it
    // gives an app a key of its own. `own_keys`: a key with the admin
    // scope on tables makes keys within them (`/v1/keys/own`).
    j["auth"] = json!({"token": app.token.is_some(), "cluster_token": app.cluster.is_some(), "keys": !app.svc.keys.is_empty(), "open": !app.guarded() && app.svc.keys.is_empty(), "app_keys": true, "own_keys": true, "limited_feed": true, "roles": true});
    j["cluster"] = app.svc.identity.json();
    let caller = keys::caller();
    // ops: what this key may do, by tier ([`crate::ops::may`]); an agent
    // by its requests (MCP, x-chloedb-agent-*) is one here too.
    let mut may = crate::ops::may(&w);
    if crate::ops::is_agent(&w, caller.as_ref()) {
        may["agent"] = json!(true);
        may["approve_changes"] = json!(false);
        may["reason_required"] = json!(true);
    }
    j["may"] = may;
    j["caller"] = json!(caller);
    // The presets a key limited to tables can be made from.
    j["presets"] = json!(keys::PRESETS.iter().map(|(n, w)| json!({"name": n, "what": w})).collect::<Vec<_>>());
    AxJson(j)
}

async fn list_keys(State(app): State<App>) -> AxJson<Json> {
    // The keys, then the revoked ones (with revoked_ms, revoked_by).
    let mut keys = app.svc.keys.list();
    let revoked = app.svc.keys.revoked();
    let n = revoked.len();
    keys.extend(revoked);
    AxJson(json!({"keys": keys, "revoked": n, "file": app.svc.keys.file().map(|p| p.display().to_string()), "peers": app.svc.keys.peers()}))
}

/// `POST /v1/admin/keys/:id/rotate {"grace_minutes": 60}`: a new key of
/// the same name, scope and owner (its secret in this answer only); the
/// old one expires after the grace period.
async fn rotate_key(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, b: Option<AxJson<Json>>) -> Result<Response, ApiError> {
    let b = b.map_or(Json::Null, |AxJson(b)| b);
    let grace = match b.get("grace_minutes") {
        None | Some(Json::Null) => 60.0,
        Some(v) => v.as_f64().filter(|m| (0.0..=10_080.0).contains(m)).ok_or_else(|| ApiError::bad("grace_minutes is a number of minutes from 0 to 10080 (a week)"))?,
    };
    let (key, secret, old) = app.svc.keys.rotate(&id, (grace * 60_000.0) as u64, app.guarded())?;
    let by = keys::audit_name(&who(w));
    tracing::info!(key = %key.id, old = %old.id, name = %key.name, by = %by, "API key rotated");
    keys::note_change(keys::Change { object: Some(format!("key {}", key.name)), before: Some(json!({"id": old.id})), after: Some(json!({"id": key.id, "old_expires_ms": old.expires_ms})), ..Default::default() });
    let detail = format!("rotated key {} ({} in place of {}, which expires in {grace} min)", key.name, key.id, old.id);
    Ok(audited(json!({"key": key.json(), "secret": secret, "old": old.json()}), detail))
}

/// `POST /v1/admin/keys/:id/budget {"rows_read": 1000000, ...}`: lowers
/// the statement budget of key `:id` (its id or name) and of the keys it
/// made ([`keys::Keys::lower_budget`]); answers the budget it has now.
async fn lower_key_budget(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let asked = chloedb::query::budget::Budget::from_json(&b)?;
    let Some(now) = app.svc.keys.lower_budget(&id, asked)? else { return Err(ApiError { code: None, status: 404, message: format!("no key {id}") }) };
    let by = keys::audit_name(&who(w));
    keys::note_change(keys::Change { object: Some(format!("key {id}")), after: Some(json!({"budget": now})), ..Default::default() });
    tracing::info!(key = %id, by = %by, budget = %now.spec(), "API key budget lowered");
    Ok(audited(json!({"key": id, "budget": now}), format!("lowered the statement budget of key {id} to {}", if now.is_none() { "none".to_string() } else { now.spec() })))
}

/// `{"name": ..., "scope": "feed" | "read" | "write" | "admin",
/// "expires_days": n}`: a new key; its secret is in this answer only.
async fn create_key(State(app): State<App>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    // A misspelt field would otherwise be left out without a word, and a
    // key without its limits reads every table: refuse what is not a
    // field of a key.
    const FIELDS: &[&str] = &["name", "scope", "tables", "routes", "apps", "preset", "prefix", "expires_days", "expires_minutes", "agent", "owner", "note", "on_behalf_of", "reason", "budget"];
    let Json::Object(fields) = &b else { return Err(ApiError::bad("a key is a JSON object: {\"name\": ..., \"scope\": ...}")) };
    if let Some(k) = fields.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(ApiError::bad(format!("{k:?} is not a field of a key: {}", FIELDS.join(", "))));
    }
    if b.get("prefix").is_some_and(|p| !p.is_null()) && b.get("preset").is_none_or(Json::is_null) {
        return Err(ApiError::bad("prefix is the tables of a preset: give a \"preset\" (GET /v1/whoami lists them), or limit the key with \"tables\": {\"team_*\": \"write\"}"));
    }
    let name = body_str(&b, "name")?;
    // `"tables": {"crawl_*": "admin", "*": "read"}`: a key limited to
    // those ([`keys::Grant`]), whose scope is the most they give (and no
    // more than `scope`, when given).
    let mut tables = match b.get("tables") {
        None | Some(Json::Null) => None,
        Some(t) => Some(keys::grants_of(t)?),
    };
    // `"routes": ["fleet"]`, `"apps": ["sheets"]`: what a key limited to
    // tables reaches beyond its tables ([`keys::Allow`]); `"preset":
    // "oncall", "prefix": "netops_*"`: the grants and allowance of a job
    // ([`keys::preset`]), kept as those, so that a server that knows no
    // presets reads the key as it would one given them by hand.
    let mut allow = keys::Allow::of(b.get("routes"), b.get("apps"))?;
    if let Some(p) = b.get("preset").filter(|p| !p.is_null()) {
        let p = p.as_str().ok_or_else(|| ApiError::bad("preset is a preset's name"))?;
        if tables.is_some() || !allow.routes.is_empty() || !allow.apps.is_empty() {
            return Err(ApiError::bad("a preset gives the key its tables, routes and apps: leave them out, or give them without a preset"));
        }
        let prefix = b.get("prefix").and_then(Json::as_str).ok_or_else(|| ApiError::bad("a preset needs a prefix, the tables it is for, as \"prefix\": \"team_*\""))?;
        let (g, a) = keys::preset(p, prefix)?;
        tables = Some(g);
        allow = a;
    }
    let asked = b.get("scope").and_then(|v| v.as_str()).unwrap_or(if tables.is_some() { "admin" } else { "read" });
    // ops: `ops` and `propose` are roles over a read key ([`keys::Role`]).
    let role = keys::Role::parse(asked);
    let scope = match role {
        Some(_) => Scope::Read,
        None => Scope::parse(asked).ok_or_else(|| ApiError::bad("scope is feed, read, write, admin, ops or propose"))?,
    };
    let expires = match (b.get("expires_days"), b.get("expires_minutes")) {
        (Some(d), Some(m)) if !d.is_null() && !m.is_null() => return Err(ApiError::bad("say expires_days or expires_minutes, not both")),
        (Some(v), _) if !v.is_null() => Some(telemetry::now_ms() + v.as_f64().filter(|d| *d > 0.0).ok_or_else(|| ApiError::bad("expires_days is a number of days above 0"))?.mul_add(86_400_000.0, 0.0) as u64),
        // An agent's key for one task: from 5 minutes.
        (_, Some(v)) if !v.is_null() => Some(telemetry::now_ms() + v.as_f64().filter(|m| *m >= 5.0).ok_or_else(|| ApiError::bad("expires_minutes is a number of minutes, 5 at least"))?.mul_add(60_000.0, 0.0) as u64),
        _ => None,
    };
    let agent = match b.get("agent") {
        None | Some(Json::Null) => role.is_some(),
        Some(v) => v.as_bool().ok_or_else(|| ApiError::bad("agent is true or false"))? || role.is_some(),
    };
    let open = !app.guarded() && app.svc.keys.is_empty();
    // A shard, or a copy in replication, whose callers send no credential.
    let in_cluster = app.svc.slots().is_some() || app.svc.role() != crate::pb::Role::Standalone;
    let text = |k: &str| b.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty()).map(|v| v.chars().take(200).collect::<String>());
    // What one statement it sends may cost ([`chloedb::query::budget`]).
    let budget = b.get("budget").map(chloedb::query::budget::Budget::from_json).transpose()?.filter(|b| !b.is_none());
    let more = keys::AgentKey { role, agent, on_behalf_of: text("on_behalf_of"), allow: Some(allow), budget };
    let (key, secret) = app.svc.keys.create_full(name, scope, tables, expires, text("owner"), text("note"), more, app.guarded(), in_cluster)?;
    let scope = key.scope_name();
    let limits = key.tables.as_deref().map(keys::grants_json);
    keys::note_change(keys::Change {
        object: Some(format!("key {}", key.name)),
        after: Some(json!({"id": key.id, "scope": scope, "tables": limits, "allow": key.allow.as_ref().map(keys::Allow::json), "owner": key.owner, "expires_ms": key.expires_ms, "agent": key.agent, "on_behalf_of": key.on_behalf_of, "budget": key.budget})),
        ..Default::default()
    });
    tracing::info!(key = %key.id, name = %key.name, scope, limited = key.tables.is_some(), agent = key.agent, "API key created");
    let detail = match &key.tables {
        Some(t) => format!("created key {} ({}, limited to {}{})", key.name, key.id, keys::grants_text(t), key.allow.as_ref().map_or(String::new(), |a| format!("{}{}{}", a.preset.as_ref().map_or(String::new(), |p| format!("; preset {p}")), if a.routes.is_empty() { String::new() } else { format!("; routes {}", a.routes.join(", ")) }, if a.apps.is_empty() { String::new() } else { format!("; apps {}", a.apps.join(", ")) }))),
        None => format!("created key {} ({}, {}{})", key.name, key.id, scope, if key.agent { ", an agent's" } else { "" }),
    };
    Ok(audited(json!({"key": key.json(), "secret": secret, "closed": open}), detail))
}

/// `GET /v1/keys/own`: the keys the calling key made
/// ([`keys::Keys::create_delegated`]).
async fn own_keys(State(app): State<App>, w: Option<axum::Extension<Principal>>) -> Res {
    let by = who(w);
    let keys: Vec<Json> = by.key.as_deref().map(|id| app.svc.keys.delegated(id)).unwrap_or_default().iter().map(keys::Key::json).collect();
    Ok(AxJson(json!({"keys": keys, "max": keys::DELEGATED_MAX})))
}

/// `POST /v1/keys/own {"name": "app-x1y2z3", "tables": {"app_x1y2z3_*":
/// "write"}, "note": ...}`: a key the calling key makes, limited to tables
/// it has the admin scope on, named `<its name>/<name>`, which opens
/// nothing once the calling key is revoked or expires; one of that name it
/// made before is revoked. Its secret is in this answer only. An app
/// engine gives each of its apps such a key, so that an app's queries are
/// held to its own tables by the server rather than by the engine.
async fn create_own_key(State(app): State<App>, w: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let by = who(w);
    let name = body_str(&b, "name")?;
    let tables = keys::grants_of(b.get("tables").ok_or_else(|| ApiError::bad("tables names what the key may touch: {\"app_x_*\": \"write\"}"))?)?;
    let note = b.get("note").and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty()).map(|v| v.chars().take(200).collect::<String>());
    let in_cluster = app.svc.slots().is_some() || app.svc.role() != crate::pb::Role::Standalone;
    let (key, secret) = app.svc.keys.create_delegated(&by, name, tables, note, in_cluster && !app.guarded())?;
    let limits = key.tables.as_deref().map(keys::grants_json);
    keys::note_change(keys::Change { object: Some(format!("key {}", key.name)), after: Some(json!({"id": key.id, "scope": key.scope.name(), "tables": limits, "owner": key.owner, "parent": key.parent})), ..Default::default() });
    tracing::info!(key = %key.id, name = %key.name, by = %by.name, "API key made by a key");
    let detail = format!("key {} made key {} ({}, limited to {})", by.name, key.name, key.id, key.tables.as_deref().map(keys::grants_text).unwrap_or_default());
    Ok(audited(json!({"key": key.json(), "secret": secret}), detail))
}

/// `DELETE /v1/keys/own/:id`: revokes a key the calling key made.
async fn revoke_own_key(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>) -> Result<Response, ApiError> {
    let by = who(w);
    let Some(k) = by.key.as_deref().and_then(|me| app.svc.keys.delegated(me).into_iter().find(|k| k.id == id)) else {
        return Err(ApiError { code: None, status: 404, message: format!("no key {id} made by {:?}", by.name) });
    };
    app.svc.keys.revoke_by(&id, app.guarded(), Some(keys::audit_name(&by)))?;
    keys::note_change(keys::Change { object: Some(format!("key {}", k.name)), before: Some(json!({"id": id})), ..Default::default() });
    tracing::info!(key = %id, by = %by.name, "API key made by a key revoked");
    Ok(audited(json!({"revoked": id}), format!("key {} revoked key {} ({id})", by.name, k.name)))
}

async fn revoke_key(State(app): State<App>, w: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>) -> Result<Response, ApiError> {
    let name = app.svc.keys.list().into_iter().find(|k| k["id"] == id.as_str()).and_then(|k| k["name"].as_str().map(String::from));
    match app.svc.keys.revoke_by(&id, app.guarded(), Some(keys::audit_name(&who(w))))? {
        true => {
            keys::note_change(keys::Change { object: name.map(|n| format!("key {n}")), before: Some(json!({"id": id})), ..Default::default() });
            tracing::info!(key = %id, "API key revoked");
            Ok(audited(json!({"revoked": id}), format!("revoked key {id}")))
        }
        false => Err(ApiError { code: None, status: 404, message: format!("no key {id}") }),
    }
}

/// `GET /v1/admin/keys/clients?hours=24`: which clients sent each key
/// over the last hours, every coordinator's (AC-4): a rotation's "move
/// the clients" says which.
async fn key_clients(State(app): State<App>, Query(q): Params, headers: HeaderMap) -> Res {
    let hours = param_u64(&q, "hours", 24)?.clamp(1, 168);
    let now = telemetry::now_ms();
    let since = now.saturating_sub(hours * 3_600_000);
    let started = now.saturating_sub(app.svc.started.elapsed().map_or(0, |d| d.as_millis() as u64));
    let mut mine = app.svc.query_log.clients.json(since);
    mine["since_ms"] = json!(since.max(started));
    mine["hours"] = json!(hours);
    let local = q.get("local").is_some_and(|v| v == "1" || v == "true") || crate::peers::from_peer(&headers).is_some();
    let Some(me) = crate::peers::this(&app.svc).filter(|_| !local) else { return Ok(AxJson(mine)) };
    let answers = crate::peers::get_all(&app.svc, &format!("/v1/admin/keys/clients?hours={hours}&local=1")).await;
    let mut parts = vec![(me.name.clone(), mine)];
    parts.extend(answers.iter().filter_map(|(p, a)| a.as_ref().ok().map(|j| (p.name.clone(), j.clone()))));
    let mut j = telemetry::KeyClients::merge(&parts);
    // From when every coordinator that answered counted.
    j["since_ms"] = json!(parts.iter().filter_map(|(_, j)| j["since_ms"].as_u64()).max());
    j["hours"] = json!(hours);
    j["coordinators"] = crate::peers::reach(&app.svc, &answers);
    Ok(AxJson(j))
}

/// `GET /v1/admin/audit?kind=change&from=&to=&who=&q=&limit=`: audited
/// requests, the newest first; from the log store for what memory no
/// longer holds. On a coordinator, every coordinator's (R2-data-tools-4:
/// each keeps its own log), each entry naming its `coordinator`, and
/// `coordinators` saying which answered; `?local=1` (or a call from
/// another coordinator) answers this one's only.
async fn audit_log(State(app): State<App>, Query(q): Params, headers: HeaderMap) -> Res {
    let limit = param_u64(&q, "limit", 200)?.clamp(1, keys::AUDIT as u64) as usize;
    let to = param_u64(&q, "to", u64::MAX)?;
    let from = param_u64(&q, "from", 0)?;
    let kinds: Vec<&str> = q.get("kind").map(|k| k.split(',').map(str::trim).filter(|k| !k.is_empty()).collect()).unwrap_or_default();
    if let Some(k) = kinds.iter().find(|k| ![keys::CHANGE, keys::REFUSAL, keys::NOTE_KIND].contains(k)) {
        return Err(ApiError::bad(format!("kind is change, refusal or note, not {k:?}")));
    }
    let by = q.get("who").map(|w| w.trim().to_lowercase()).filter(|w| !w.is_empty());
    let text = q.get("q").map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty());
    let fits = |e: &Json| {
        let s = |k: &str| e[k].as_str().unwrap_or("").to_lowercase();
        let ts = e["ts_ms"].as_u64().unwrap_or(0);
        ts >= from
            && ts <= to
            && (kinds.is_empty() || kinds.contains(&e["kind"].as_str().unwrap_or("")))
            && by.as_ref().is_none_or(|w| s("who").contains(w.as_str()) || s("key") == *w)
            && text.as_ref().is_none_or(|t| ["detail", "route", "object", "reason", "who", "client", "addr"].iter().any(|k| s(k).contains(t.as_str())))
    };
    let all = app.svc.audit.latest(keys::AUDIT);
    // From when memory holds every entry: since the server started, or
    // its oldest once it holds as many as it keeps.
    let up_ms = app.svc.started.elapsed().map_or(0, |d| d.as_millis() as u64);
    let held = match all.last() {
        Some(e) if all.len() >= keys::AUDIT => e.ts_ms,
        _ => telemetry::now_ms().saturating_sub(up_ms),
    };
    let mut entries: Vec<Json> = all.iter().map(|e| {
        let mut j = json!(**e);
        j["kind"] = json!(e.kind());
        j
    }).filter(|j| fits(j)).take(limit).collect();
    let mut source = vec!["memory"];
    let mut error = None;
    if let (Some(store), true) = (&app.svc.log_store, entries.len() < limit && from < held) {
        let and = (kinds.len() == 1).then(|| format!("kind = '{}'", kinds[0]));
        let want = ((limit - entries.len()) * 4).min(keys::AUDIT);
        match store.kept_rows(crate::logstore::AUDIT, from, held.saturating_sub(1).min(to), and, want).await {
            Ok(rows) => {
                let more = rows.iter().filter_map(|r| r["entry"].as_str().and_then(|t| serde_json::from_str::<Json>(t).ok())).filter(|j| fits(j));
                entries.extend(more.take(limit - entries.len()));
                source.push("log_store");
            }
            Err(e) => error = Some(format!("the log store did not answer: {}", e.message)),
        }
    }
    let mut j = json!({"entries": entries, "file": app.svc.audit.path().map(|p| p.display().to_string()), "source": source, "memory_from_ms": held});
    if let Some(e) = error {
        j["error"] = json!(e);
    }
    let local = q.get("local").is_some_and(|v| v == "1" || v == "true") || crate::peers::from_peer(&headers).is_some();
    if let (false, Some(me)) = (local, crate::peers::this(&app.svc)) {
        let mut ask: Vec<String> = q.iter().filter(|(k, _)| k.as_str() != "local").map(|(k, v)| format!("{}={}", crate::peers::encode(k), crate::peers::encode(v))).collect();
        ask.sort();
        let answers = crate::peers::get_all(&app.svc, &format!("/v1/admin/audit?{}", ask.join("&"))).await;
        let mut all: Vec<Json> = std::mem::take(j["entries"].as_array_mut().expect("entries"));
        for e in &mut all {
            e["coordinator"] = json!(me.name);
        }
        for (p, a) in &answers {
            for e in a.as_ref().ok().and_then(|a| a["entries"].as_array()).into_iter().flatten() {
                let mut e = e.clone();
                e["coordinator"] = json!(p.name);
                all.push(e);
            }
        }
        // The newest first, over every coordinator.
        all.sort_by(|a, b| b["ts_ms"].as_u64().cmp(&a["ts_ms"].as_u64()));
        all.truncate(limit);
        j["entries"] = Json::Array(all);
        j["coordinators"] = crate::peers::reach(&app.svc, &answers);
    }
    Ok(AxJson(j))
}

// ----- alerts (alerts agent): rules, states, acknowledgements, silences,
// notes, routes and backtests ([`crate::alerts`])

/// The rules with their states, the latest changes and the routes.
async fn alerts(State(app): State<App>, Query(q): Params) -> AxJson<Json> {
    // alerts2: `?brief=1` for the bell.
    if q.get("brief").is_some_and(|v| v == "1" || v == "true") {
        return AxJson(app.svc.alerts.brief_json());
    }
    AxJson(app.svc.alerts.json())
}

/// The name a change is made by: the request's key.
fn by(who: &Principal) -> String {
    keys::audit_name(who)
}

/// One rule's state with its events: those in memory, and those of the
/// last 7 days in the log store's `alerts` table.
async fn alert(State(app): State<App>, axum::extract::Path(id): axum::extract::Path<String>) -> Res {
    let mut j = app.svc.alerts.one(&id)?;
    if let Some(store) = app.svc.log_store.clone() {
        let since = telemetry::now_ms().saturating_sub(crate::alerts::KEEP_DAYS * 86_400_000);
        match store.alert_events(&id, since, 500).await {
            Ok(rows) => {
                let mut events: Vec<Json> = j["events"].as_array().cloned().unwrap_or_default();
                let seen: std::collections::HashSet<(u64, String)> = events.iter().map(|e| (e["ts_ms"].as_u64().unwrap_or(0), e["state"].as_str().unwrap_or("").to_string())).collect();
                events.extend(rows.into_iter().filter(|e| !seen.contains(&(e["ts_ms"].as_u64().unwrap_or(0), e["state"].as_str().unwrap_or("").to_string()))));
                events.sort_by(|a, b| b["ts_ms"].as_u64().cmp(&a["ts_ms"].as_u64()));
                j["events"] = json!(events);
                j["events_source"] = json!("log_store");
            }
            Err(e) => j["events_error"] = json!(e.message),
        }
    }
    Ok(AxJson(j))
}

async fn put_alert_rules(State(app): State<App>, p: Option<axum::Extension<Principal>>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let rules: Vec<crate::alerts::Rule> = serde_json::from_value(b.get("rules").cloned().ok_or_else(|| ApiError::bad("the body needs \"rules\""))?).map_err(|e| ApiError::bad(format!("bad rules: {e}")))?;
    let n = rules.len();
    let who = by(&who(p));
    let (events, before, after) = app.svc.alerts.set_rules(rules, &who)?;
    let (before, after) = crate::alerts::changed(&before, &after);
    let what = crate::alerts::change_text(&before, &after);
    keys::note_change(keys::Change { object: Some("alert rules".into()), before: Some(before), after: Some(after), ..Default::default() });
    crate::alerts::deliver(&app.svc, events);
    // alerts2: every copy runs the each_copy rules as edited here, and every
    // coordinator has them; the answer says which copies did not take them.
    let (rules_ms, rules_by) = app.svc.alerts.rules_version();
    app.svc.alerts.tell_peers(json!({"rules": app.svc.alerts.rules(), "rules_ms": rules_ms, "rules_by": rules_by}));
    let mut j = app.svc.alerts.json();
    if app.svc.alerts.is_coordinator() {
        j["pushed"] = crate::alerts::push_rules(&app.svc, None, &who).await;
    }
    Ok(audited(j, format!("{n} alert rules: {what}")))
}

/// alerts2: what this coordinator holds of its alerts, for another one to
/// merge (rules and their version; acknowledgements, silences, notes and
/// pages).
async fn alert_peer_state(State(app): State<App>) -> AxJson<Json> {
    AxJson(app.svc.alerts.peer_state())
}

/// alerts2: merges what another coordinator tells (as it answers
/// `GET /v1/alerts/peer`, and routes); it is not told on again.
/// Only with the cluster's token (or the server's), as another coordinator
/// sends it: an admin key could otherwise replace the rules, routes and
/// silences unaudited.
async fn alert_peer_merge(State(app): State<App>, p: Option<axum::Extension<Principal>>, headers: HeaderMap, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    if !who(p).root {
        return Err(ApiError { code: None, status: 403, message: "only another coordinator tells this, with the cluster's token: an API key may not".into() });
    }
    let from = crate::peers::from_peer(&headers).ok_or_else(|| ApiError::bad("only another coordinator tells this (the header x-strata-peer names it)"))?;
    let events = app.svc.alerts.merge(&b, &from)?;
    let n = events.len();
    crate::alerts::deliver(&app.svc, events);
    // Not audited (R2-scenarios-10): the coordinator it came from audited
    // the person's change, and every coordinator's audit log is merged.
    Ok(AxJson(json!({"merged": n})).into_response())
}

/// Notes for the audit entry what a request changed of alert `id`: its
/// fields `fields` before and after.
fn alert_change(id: &str, before: &Json, after: &Json, fields: &[&str]) {
    let pick = |j: &Json| Json::Object(fields.iter().map(|k| (k.to_string(), j.get(*k).cloned().unwrap_or(Json::Null))).collect());
    keys::note_change(keys::Change { object: Some(format!("alert {id}")), before: Some(pick(before)), after: Some(pick(after)), ..Default::default() });
}

async fn ack_alert(State(app): State<App>, p: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, b: Option<AxJson<Json>>) -> Result<Response, ApiError> {
    let note = b.and_then(|AxJson(b)| b.get("note").and_then(Json::as_str).map(String::from));
    let before = app.svc.alerts.one(&id)?;
    let e = app.svc.alerts.ack(&id, &by(&who(p)), note)?;
    keys::note(format!("acknowledged alert {id}"));
    crate::alerts::deliver(&app.svc, vec![e]);
    let after = app.svc.alerts.one(&id)?;
    alert_change(&id, &before, &after, &["state", "ack"]);
    Ok(evented(AxJson(after).into_response()))
}

async fn silence_alert(State(app): State<App>, p: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let minutes = b.get("minutes").and_then(Json::as_u64).ok_or_else(|| ApiError::bad("the body needs \"minutes\""))?;
    let reason = b.get("reason").and_then(Json::as_str).unwrap_or_default().to_string();
    // ops: an agent silences an alert for two hours at most.
    if let Some(w) = p.as_ref().map(|e| &e.0) {
        if crate::ops::is_agent(w, keys::caller().as_ref()) && minutes > crate::ops::AGENT_SILENCE_MIN {
            return Err(ApiError::coded(400, "limit_exceeded", format!("an agent silences an alert for {} min at most, not {minutes}", crate::ops::AGENT_SILENCE_MIN)));
        }
    }
    let before = app.svc.alerts.one(&id)?;
    let e = app.svc.alerts.silence(&id, &by(&who(p)), minutes, &reason)?;
    keys::note(format!("silenced alert {id} for {minutes} min: {reason}"));
    crate::alerts::deliver(&app.svc, vec![e]);
    let after = app.svc.alerts.one(&id)?;
    alert_change(&id, &before, &after, &["silenced"]);
    Ok(evented(AxJson(after).into_response()))
}

async fn unsilence_alert(State(app): State<App>, p: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>) -> Result<Response, ApiError> {
    let before = app.svc.alerts.one(&id)?;
    let e = app.svc.alerts.unsilence(&id, &by(&who(p)))?;
    keys::note(format!("lifted the silence of alert {id}"));
    crate::alerts::deliver(&app.svc, vec![e]);
    let after = app.svc.alerts.one(&id)?;
    alert_change(&id, &before, &after, &["silenced"]);
    Ok(evented(AxJson(after).into_response()))
}

async fn note_alert(State(app): State<App>, p: Option<axum::Extension<Principal>>, axum::extract::Path(id): axum::extract::Path<String>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let text = b.get("text").and_then(Json::as_str).ok_or_else(|| ApiError::bad("the body needs \"text\""))?;
    let e = app.svc.alerts.note(&id, &by(&who(p)), text)?;
    keys::note(format!("noted on alert {id}: {}", crate::obs::truncate(text, 200)));
    // A note, as a note on the Timeline is ([`events_note`]).
    keys::note_change(keys::Change { kind: Some(keys::NOTE_KIND), object: Some(format!("alert {id}")), after: Some(json!({"note": text})), ..Default::default() });
    crate::alerts::deliver(&app.svc, vec![e]);
    Ok(evented(AxJson(app.svc.alerts.one(&id)?).into_response()))
}

async fn alert_routes(State(app): State<App>) -> AxJson<Json> {
    AxJson(json!({"routes": app.svc.alerts.routes_json()}))
}

/// Replaces the routes (a route without its `url` keeps the one it had),
/// audited with the routes before and after (their URLs without paths).
async fn put_alert_routes(State(app): State<App>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let routes: Vec<crate::alerts::Route> = serde_json::from_value(b.get("routes").cloned().ok_or_else(|| ApiError::bad("the body needs \"routes\""))?).map_err(|e| ApiError::bad(format!("bad routes: {e}")))?;
    let before = app.svc.alerts.routes_json();
    app.svc.alerts.set_routes(routes)?;
    // alerts2: every coordinator sends through the same routes.
    app.svc.alerts.tell_peers(json!({"routes": app.svc.alerts.routes()}));
    let after = app.svc.alerts.routes_json();
    let names: Vec<&str> = after.iter().filter_map(|r| r["name"].as_str()).collect();
    keys::note_change(keys::Change { object: Some("alert routes".into()), before: Some(json!(before)), after: Some(json!(after)), ..Default::default() });
    Ok(audited(json!({"routes": app.svc.alerts.routes_json()}), format!("alert routes: {}", if names.is_empty() { "none".to_string() } else { names.join(", ") })))
}

/// Posts a test message to a route, and says how it went.
async fn test_alert_route(State(app): State<App>, p: Option<axum::Extension<Principal>>, axum::extract::Path(name): axum::extract::Path<String>) -> Result<Response, ApiError> {
    let j = app.svc.alerts.test_route(&name, &by(&who(p))).await?;
    let ok = j["ok"] == true;
    Ok(audited(j, format!("sent a test message to alert route {name}{}", if ok { "" } else { " (it failed)" })))
}

/// Replays a rule over up to 7 days ([`crate::alerts::backtest`]).
async fn alert_backtest(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    crate::alerts::backtest(&app.svc, &b).await.map(AxJson)
}

// ----- end alerts

#[cfg(test)]
mod tests {
    use super::*;
    use chloedb::value::Value;

    #[test]
    fn shard_addresses_are_the_admins() {
        use chloedb::query::steps::Step;
        let steps = || vec![Step { op: "shard".into(), detail: "shard 0 query, http://s0-0.svc:7701".into(), ..Step::default() }, Step { op: "bm25".into(), detail: "title, a, b".into(), ..Step::default() }];
        let mut admin = steps();
        hide_hosts(&Principal::open(), &mut admin);
        assert_eq!(admin, steps());
        let reader = Principal { name: "r".into(), key: Some("k".into()), scope: Scope::Read, root: false, tables: None, role: None, agent: false, on_behalf_of: None, allow: None };
        let mut seen = steps();
        hide_hosts(&reader, &mut seen);
        assert_eq!((seen[0].detail.as_str(), seen[1].detail.as_str()), ("shard 0 query", "title, a, b"));
        let limited = Principal { scope: Scope::Admin, tables: Some(vec![keys::Grant::parse("x_*=admin").unwrap()].into()), ..reader };
        let mut seen = steps();
        hide_hosts(&limited, &mut seen);
        assert_eq!(seen[0].detail, "shard 0 query");
    }

    /// Mining synonyms is an admin's; the routes that only read a search's
    /// understanding stay a reader's.
    #[test]
    fn mining_synonyms_takes_an_admin_key() {
        let post = axum::http::Method::POST;
        assert_eq!(route_scope(&post, "/v1/synonyms/mine"), Scope::Admin);
        for r in ["/v1/correct", "/v1/understand", "/v1/classify", "/v1/derive/preview"] {
            assert_eq!(route_scope(&post, r), Scope::Read, "{r}");
        }
    }

    /// A read by key's answer, written directly, is the JSON the tree
    /// built from the same rows was: every kind of value, a key not found,
    /// a row shorter than the columns and a column named twice.
    #[test]
    fn key_rows_written_as_the_tree_was() {
        let columns: Vec<String> = ["id", "v", "f", "nan", "t", "list", "b", "z", "a", "id"].iter().map(|s| s.to_string()).collect();
        let row = |id: i64| {
            vec![
                Value::Int(id),
                Value::Vector(vec![0.1f32, -2.5, 1e-7].into()),
                Value::Float(1.25),
                Value::Float(f64::NAN),
                Value::Timestamp(1_790_000_000_000_000),
                Value::List(vec![Value::Str("x\"y".into()), Value::Null, Value::Int(-3)]),
                Value::Bool(true),
                Value::Str("é\n".into()),
                Value::Null,
                Value::Str("last".into()),
            ]
        };
        let mut short = row(2);
        short.truncate(4);
        let got = crate::service::KeyRows { table: "docs".into(), columns: columns.clone(), rows: vec![Some((row(1), 7)), None, Some((short, 9))] };
        let seq_nos: Vec<Json> = got.rows.iter().map(|r| r.as_ref().map_or(Json::Null, |(_, s)| json!(s))).collect();
        let rows: Vec<Json> = got.rows.iter().map(|r| r.as_ref().map_or(Json::Null, |(row, _)| key_row(&got.columns, row))).collect();
        let tree = json!({"table": got.table, "columns": got.columns, "rows": rows, "seq_nos": seq_nos, "took_ms": 1.5});
        let direct = serde_json::to_string(&KeyRowsJson { got: &got, took_ms: 1.5 }).unwrap();
        assert_eq!(direct, serde_json::to_string(&tree).unwrap());
    }
}

// ----- connectors (crate::connectors)

/// Who a connectors request is from, for the calls it makes with the
/// caller's credential.
fn connector_caller(app: &App, headers: &HeaderMap, w: &Principal) -> crate::connectors::Caller {
    let auth = headers.get(header::AUTHORIZATION).cloned().or_else(|| headers.get(header::HeaderName::from_static(crate::hub::VIA_PROXY_AUTH)).cloned());
    let in_cluster = app.svc.slots().is_some() || app.svc.role() != crate::pb::Role::Standalone;
    crate::connectors::Caller { auth, guarded: app.guarded(), in_cluster, who: w.name.clone() }
}

async fn connectors_list(State(app): State<App>, w: Option<axum::Extension<Principal>>, headers: HeaderMap) -> Res {
    let c = connector_caller(&app, &headers, &who(w));
    Ok(AxJson(crate::connectors::list_json(&app.svc, &c).await?))
}

async fn connectors_one(State(app): State<App>, axum::extract::Path(name): axum::extract::Path<String>) -> Res {
    Ok(AxJson(crate::connectors::one_json(&app.svc, &name).await?))
}

async fn connectors_create(State(app): State<App>, w: Option<axum::Extension<Principal>>, headers: HeaderMap, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let c = connector_caller(&app, &headers, &who(w));
    let r = crate::connectors::create(&app.svc, &c, &b).await?;
    let detail = format!("created connector {} ({} into {})", r["connector"]["name"].as_str().unwrap_or_default(), r["connector"]["kind"].as_str().unwrap_or_default(), r["connector"]["target"].as_str().unwrap_or_default());
    Ok(audited(r, detail))
}

async fn connectors_update(State(app): State<App>, w: Option<axum::Extension<Principal>>, headers: HeaderMap, axum::extract::Path(name): axum::extract::Path<String>, AxJson(b): AxJson<Json>) -> Result<Response, ApiError> {
    let c = connector_caller(&app, &headers, &who(w));
    let r = crate::connectors::update(&app.svc, &c, &name, &b).await?;
    Ok(audited(r, format!("changed connector {name}")))
}

async fn connectors_delete(State(app): State<App>, w: Option<axum::Extension<Principal>>, headers: HeaderMap, axum::extract::Path(name): axum::extract::Path<String>) -> Result<Response, ApiError> {
    let c = connector_caller(&app, &headers, &who(w));
    let r = crate::connectors::delete(&app.svc, &c, &name).await?;
    Ok(audited(r, format!("deleted connector {name}")))
}

async fn connectors_rotate(State(app): State<App>, w: Option<axum::Extension<Principal>>, headers: HeaderMap, axum::extract::Path(name): axum::extract::Path<String>) -> Result<Response, ApiError> {
    let c = connector_caller(&app, &headers, &who(w));
    let r = crate::connectors::rotate(&app.svc, &c, &name).await?;
    Ok(audited(r, format!("rotated the push key of connector {name}")))
}

async fn connectors_run(State(app): State<App>, axum::extract::Path(name): axum::extract::Path<String>) -> Res {
    Ok(AxJson(crate::connectors::run_now(&app.svc, &name).await?))
}

async fn connectors_test(State(app): State<App>, AxJson(b): AxJson<Json>) -> Res {
    Ok(AxJson(crate::connectors::test(&app.svc, &b).await?))
}

/// `POST /v1/connectors/:name/push`: any body (JSON, or a raw email).
async fn connectors_push(State(app): State<App>, headers: HeaderMap, axum::extract::Path(name): axum::extract::Path<String>, body: axum::body::Bytes) -> Res {
    let auth = headers.get(header::AUTHORIZATION).cloned();
    let ty = header_str(&headers, header::CONTENT_TYPE).unwrap_or("application/octet-stream").to_ascii_lowercase();
    Ok(AxJson(crate::connectors::push(&app.svc, &name, auth.as_ref(), &ty, &body).await?))
}
