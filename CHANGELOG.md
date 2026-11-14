# Changelog

What changed in ChloeDB (formerly Strata) and its packages, newest first.
Each release says which artefacts it covers: `chloedb-server`, the Python
package (`chloedb`), the JavaScript package (`chloedb`), the Java package,
ChloeCtx. [docs/compatibility.md](docs/compatibility.md) says what a version
promises about the file format, the shard protocol and the old names.

Every crate and package carries the release's version; between releases
changes collect under "Unreleased".

## Unreleased

- **Experimental marks.** The cloud provisioners, the hub, the autoscaler
  and the on-call agent are marked experimental: no compatibility promise
  yet. Their answers carry `x-chloedb-experimental: <feature>`,
  `GET /v1/catalog` marks their routes (`experimental`) and lists the
  features, and their command-center pages say so
  ([compatibility](docs/compatibility.md#experimental-features)).
- **Demos on a train of their own.** chloedb-netops, chloedb-swarm,
  examples/feature-store and examples/parley are tested by the new `Demos`
  workflow (on a pull request that touches one, and every night), not by
  `CI result`; a fault in one no longer holds a release. An app's manifest
  may say `"maturity": "demo"`, and the command center marks it
  ([compatibility](docs/compatibility.md#demos)).
- **One-way doors checked.** `.github/CODEOWNERS` names each door in a
  `# door:` section and adds the paths audit 4 found missing (rollup
  layouts, the substring index, analyzers, archives, tenants, the app
  engine's keys and access, the install scripts and the Dockerfiles). The
  new check `One-way doors` fails a pull request whose template box says
  "No one-way door" (or nothing) while its diff touches one, and writes a
  receipt of the doors touched into the run's summary.

### Rollups (experimental): correctness fixes (audit 4, X-A1 to X-A3, X-B4 to X-B8)

Rollups are marked experimental in the docs: their options, columns and
refresh may change in a later release.

- **One broken rollup no longer stops the others.** A server refreshes each
  rollup on its own, logs a failure by the rollup's name (again only when its
  error changes, and once when it works again), and goes on with the rest; it
  used to stop at the first failure, so every rollup listed after it went
  stale. A definition that does not read is reported the same way.
  `GET /v1/tables` gives a rollup's table a `rollup` entry: the table it reads,
  its last good refresh, its last error and the failures in a row
  (`Db::rollup_health`, `Db::refresh_rollups` in Rust).
- **What would break a rollup is refused.** `DROP TABLE` of a table a rollup
  reads (`DROP ROLLUP` first, or `DROP TABLE logs, logs_5m`), `ALTER TABLE`
  that drops or renames a column a rollup reads, and any `ALTER TABLE` of a
  rollup's own table. `DROP TABLE` of a rollup's own table drops its
  definition too.
- **The refresh window follows the clock.** A plain refresh compares the
  buckets from now less the lateness, not from the newest row: one row dated
  in the future (a device's clock) used to move the window there, so the
  present was folded only by the sweep every five minutes. Only buckets that
  hold rows are compared, and a span without rows is stepped over in one
  query: a row at 1970 or 2099 used to make every sweep walk every empty
  bucket between (27,000 count queries for a 5-minute rollup).
- **A bucket's integer key no longer wraps.** A bucket too far from 1970 for
  its key to fit (a nanosecond time stored as microseconds) is left out and
  logged instead of overwriting another bucket's row.
- **`AVG(f)` skips NULLs.** It keeps `count_f` beside `sum_f`, and its mean is
  `SUM(sum_f) / SUM(count_f)`; `COUNT(f)` is a rollup aggregate too. Its mean
  used to divide by every row, NULLs included. A rollup made before keeps its
  columns and its old mean; `DROP ROLLUP` and `CREATE ROLLUP` again for the
  new one. Such a rollup is marked `needs_chloedb_0_3_4`, which 0.3.3 refuses
  by name.
- **One refresh of a rollup at a time.** Two at once (the server's refresher
  and a `REFRESH ROLLUP`, say) could leave a group doubled and another lost,
  with counts that agreed so that no later refresh mended it.
- **`REFRESH ROLLUP name REBUILD`** folds every bucket again, whatever its
  count: an `UPDATE` keeps a bucket's count, so no plain or `FULL` refresh
  ever sees it (the docs said "when the bucket is next folded").
- `buckets_checked` in a refresh's answer counts the buckets that hold rows,
  no longer every bucket of the span.

### Relevance autopilot and query understanding (audit 4: N-C1, A1, A6, X-C1, X-C2, X-C4)

- **Auth change:** `relevance_proposals` and `relevance_judgments` are written
  by admin keys only. A write key's write or statement that changes either
  answers 403 `scope_refused`; the autopilot's own routes already needed an
  admin key.
- Applying a proposal no longer takes its row at its word. Its statement is
  applied only when it is the rank profile now with some of its numbers
  moved (a ranking of the row's own is refused, with `force` too), and its
  proof is made again on the rank log and the grades as they are, with the
  options it was measured with; when that proof does not propose it, Apply
  is refused unless `force`. A proof now names its options (`options`).
- The autopilot no longer proposes a change that made fewer held-out
  queries better than worse: the two-sided sign test passed 2 queries up by
  0.5 against 12 down by 0.01. Such a run is `not_significant`.
- Query understanding reads at most 20,001 values of a field it was not
  told to read (it read 200,000 and then left the field out), reads each
  dictionary by one query at a time while the others use the one before,
  and uses a dictionary up to a second old on a table that takes writes.
  It understands a text's first 32 words and uses its first 16 runs, and
  counts each run with one query instead of two (runs of the same words and
  values share it).
- A `dict:<table>.<field>` derived field on its own table is refused at
  `CREATE TABLE` and `ALTER TABLE ... ADD COLUMN` (each write read the
  whole table again); a coordinator refuses `dict:` derived fields, whose
  shards would tag rows from their own share of the dictionary. Tables that
  have one keep it.

## 0.3.3

The afternoon audit's fixes, a ChloeCtx inspector organised by space, and
smaller, faster rollups: `chloedb-server-v0.3.3`, `chloedb-py-v0.3.3` and
`chloedb-js-v0.3.3` (the Java package is built from source; `chloedb-crawl`,
`chloedb-sheets` and `chloectx` are attached prebuilt to the server's
release). The file format stays 3 and the shard protocol stays 21; no plan
feature is new, so servers of 0.3.2 and 0.3.3 mix in a cluster in either
order. Auth changes: a connector's push key reads nothing,
`POST /v1/admin/keys` refuses fields it does not know, the connectors'
tables are the server's own, and `env:` secrets read only variables with a
prefix (below). **A connector that reads an internal system needs
`"allow_internal": true`.**

### What changes by default

- A short read-only statement can no longer stop the server with a stack
  overflow: deep statements run on a thread with a larger stack, and
  planning stops at the request's deadline or when the client goes away
  (504 `deadline_exceeded`).
- A write that races `ALTER TABLE` is bound to the table's new columns
  again instead of failing with `schema_invalid`.
- A connector's push key (`connector:<name>`) may call only its push,
  `POST /v1/write`, `GET /v1/health` and `GET /v1/whoami`; reads answer 403
  `scope_refused`. A connector's stored and answered errors no longer carry
  its secrets.
- `POST /v1/admin/keys` refuses an unknown field with a 400, and `prefix`
  without `preset`.
- Request fields of the wrong type (`timeout_ms: "100"`) answer 400 instead
  of being read as missing, and errors outside the handlers (bad JSON, 404,
  405, 413, 415, overload) have the API's error shape.
- A published app whose owner never chose who may do what is "Visitors can
  look" (read only) until the owner picks more.
- ChloeCtx: the end of a turn no longer waits for query understanding's
  background search or answers `decision: block`; `chloectx install
  --project DIR` refuses a folder that does not exist, and `chloectx prune`
  leaves alone a store whose folder went missing in the last week.
- Network operations: `GET /v1/map/ask` writes nothing; the search that
  is kept and asks the model is `POST /v1/map/ask`.
- Rollups made from now on key their rows by number and keep their columns
  only, about 40 bytes a row; rollups made by 0.3.1 and 0.3.2 keep working
  as they are (`DROP ROLLUP` and `CREATE ROLLUP` again to get the smaller
  table). 0.3.2 cannot refresh a rollup made by 0.3.3, and refuses it by
  name (below).
- A statement that calls a stored function is held, written out, to the
  depth (1,000 levels) and size (250,000 expressions) a statement's text
  may have, and is refused past them with a 400 `query_invalid`.
- App engine: screen expressions run only in QuickJS, `claude -p` and
  `codex exec` get a reduced environment, Codex is off unless
  `STRATA_APPS_CODEX_ON=1`, and its MCP server signs in with the engine's
  agent token.

### Connectors

Connectors locked down (audit 4, X-N1, X-N2, X-N4, X-N5). Auth changes:
the connectors' tables are the server's own, and `env:` secrets read only
variables with a prefix. These change by default:

- `chloedb_connectors`, `chloedb_connector_runs` and
  `chloedb_connector_state` are written by the server only: a statement or
  write that changes them answers 403 `server_owned` for every key, an
  admin's too (the root token is still let through). Change a connector
  with `POST /v1/admin/connectors/NAME`. A write key could otherwise point a
  connector, with its credentials, at any URL, or give its run a key that
  writes every table (`target = '*'`).
- The schedule, a run and a push check each config row as the API does; a
  row that fails shows health `invalid` and does not run. Configs record
  `updated_by` (the column is added to an existing table at the next change).
- A connector reaches the public internet only. A system on a private
  network or the server's own machine needs `"allow_internal": true` in the
  connector's config; link-local addresses (cloud metadata) are refused
  even then. **Connectors made before this that read an internal system
  (a Prometheus inside the cluster, say) fail until it is set.**
- A connector's auth and headers go to its configured origin only:
  redirects are followed within it (five at most), and an `http_json` next
  page elsewhere is asked without them or any secret.
- An answer is read up to 32 MiB (`max_body_mb`, up to 1024), a Prometheus
  run keeps 20,000 records at most (ending at an earlier step when its
  series make more), and a push takes 20,000 records (413 beyond) and a
  body of 32 MiB at most.
- An `env:` secret names a variable that starts with
  `CHLOEDB_CONNECTOR_SECRET_` (`--connector-env-prefix`); another is
  refused (400) and, if kept from before, not read. A dry run
  (`POST /v1/admin/connectors/test`) takes no `env:` secret.

### SQL and the engine

#### Fixed

- A short read-only statement can no longer stop the server. A chain of 250 `UNION ALL` queries (`INTERSECT` and `EXCEPT` at 260), a 990-term `'a' || 'a' || ...`, or a `GROUP BY` of a 990-term sum used to abort the process with a stack overflow, on a single server and on a coordinator, for any key that may read. Each `UNION`, `INTERSECT` or `EXCEPT` now counts eight levels of the 1,000 a statement may nest (so up to 125 queries joined), and set operations are planned along their chain instead of recursing down it. An expression nested more than 48 levels deep runs on ChloeDB's operators instead of DataFusion, which took seconds for it and then overflowed. A statement that nests deep is planned and run on a thread with 8 MiB of stack, whichever thread called it: the server's or an application's `Db::query` or `Db::sql`. Past the limit the answer is a 400 with `error_code` `query_invalid`: "the statement nests more than 1000 levels deep (... or more than 125 queries joined by UNION, INTERSECT or EXCEPT)". Through a coordinator, a plan too deep for the shard protocol's messages is a 400 `query_invalid` too. It used to be a 502, and the shard's copy was marked as failing (audit PRF-1).
- Planning a statement stops at the request's deadline and when the client goes away, with a 504 `deadline_exceeded`. A statement longer than 2 KiB is planned on a blocking thread, so a long one no longer holds a thread that serves the other requests. `Db::query_with` and `Db::sql_with` stop planning at their `ExecOptions` deadline as well. A chain of 2,000 `WITH` results plans in 0.3 s. It took 13.5 s, past the deadline, because each result copied the rest of the statement (audit PRF-2).
- A write that runs while `ALTER TABLE` adds, renames or drops a column of its table now applies to the table's new columns, as long as it names only columns the table keeps. Before, `/v1/write` answered a 400 `schema_invalid` ("document has 4 values, table has 3 fields", not retryable), and an `INSERT`, `UPDATE` or `DELETE` failed with "table ... was altered ... while the statement ran; run it again". Now a statement whose table changed between binding and writing is bound again and run, up to ten times, on a server and in `Db::sql` (audit PRF-3).
- A call of a stored function can no longer stop the server with a stack overflow. Calls are written out after the statement's text is checked, so `SELECT f16(n) FROM t` (25 bytes), with sixteen functions each calling the one before and each body a 990-term sum, planned about 16,000 levels deep. Functions whose bodies call one another, or calls nested in arguments, `f1(f1(f1(n)))`, did the same. A call written out is now measured before it is walked into, and a statement whose calls written out nest deeper than 1,000 levels, or hold more than 250,000 expressions (a body that uses its parameter four times and calls one that does too, sixteen deep), is refused with "f2(...) written out nests more than 1000 levels deep ..." or "the calls of g1 written out hold more than 250000 expressions ...". A statement that may call a stored function is planned and run on a thread with 8 MiB of stack, however short its text (audit 4, X-B1).
- A condition of many parts over rows in memory no longer reaches DataFusion as a chain as deep as it has parts. `tags @> :tags` with a list of 2,000 values (an `IN` for each value, joined by `AND`) over 5,000 rows, or a long `WHERE` over joined rows, became a chain as deep as the list, which DataFusion walks recursively, and the process aborted. An `AND` or `OR` of up to 1,024 conditions now reaches DataFusion as a balanced tree, and a wider one runs on ChloeDB's operators, which evaluate it in a loop (audit 4, X-B2).
- A segment whose graph section holds damaged offsets is refused as corrupt when it opens. Before, offsets that passed the size checks could describe a range of petabytes, and a graph query over them aborted the process trying to allocate it (found by the nightly fuzzer, issue #485).

#### Documentation

- The query language page says what `INT` arithmetic does past 64 bits: the result is a `FLOAT`, not an error. It also says that `/` always gives a `FLOAT` and that dividing by zero gives `NULL` (audit PRF-10).

#### Changed

- ChloeDB: a rollup's first fold and its full refresh read a day of
  buckets per query (one `date_bin` GROUP BY for the counts and one for
  the fold) instead of a query per bucket. On 20 million synthetic lines
  over 30 days (a 4-core Xeon cloud container), `CREATE ROLLUP` by
  5 minutes, service and severity takes about 8 seconds and a full
  refresh a quarter of a second.
- ChloeDB: rollups made from now on key their rows with a number (the
  bucket and the group's place in it) rather than text, and keep their
  columns in the columns only (`stored = false`), not again as whole
  rows: about 40 bytes a row once merged. Rollups made by 0.3.1 and 0.3.2
  keep their text keys and go on working; `DROP ROLLUP` and `CREATE
  ROLLUP` again to get the smaller table.
- ChloeDB: a rollup made since this release keeps the option
  `needs_chloedb_0_3_3` in its definition. 0.3.2 can read such a rollup
  but not refresh it (its refresher writes text keys), and it refuses the
  definition with "no option needs_chloedb_0_3_3", so its refresh says why
  instead of serving stale buckets. Open a database whose rollups 0.3.3
  made with 0.3.3 or later (audit 4, X-I5).
- `log_series_bench rollup` prints the rollup's bytes on disk, as flushed
  and once merged.

### Server

#### Security

- A connector's errors no longer carry its secrets. A failed request is reported without reqwest's URL text, which held the query string (`?api_key=...`). Every error a connector stores or answers (`last_error`, the runs' `error`, the run and dry-run replies, the log) has every URL's query cut off and every secret's value replaced by `***`. A read key could `SELECT` them before (audit DAP-2).
- A connector's push key (`connector:<name>`) now reads nothing, as the docs said. It may call only its push, `POST /v1/write`, `GET /v1/health` and `GET /v1/whoami`; a query, `GET /v1/tables`, a read by key or an export of its own target is refused with 403 `scope_refused`. Before, it could read the table it writes (audit DAP-3).
- `POST /v1/admin/keys` refuses a field it does not know with a 400 that names it, and refuses `prefix` without `preset`. Before, a misspelt `preset` or `tables` was ignored, and the key it made read every table (audit ONB-5).

#### Fixed

- Change requests: a count of rows within twice or half of what it was at filing, or no more than 10 rows from it, no longer makes the request `plan_stale` at approval. Before, small counts had to match exactly, so a `DELETE` of 42 rows on a table taking writes could not be approved. Twice or half the rows, another table or another shard still go stale (audit CCW-4).
- `llm()`, text derived by a model, and LLM grading no longer send `temperature` to the Anthropic API. Current Claude models answer 400 to it, so every call failed (for the network operations app's Ask, "the server has no model") (audit NET-5).
- An `http_json` connector with `next` and no cursor that moves its URL on, stopped at the 20,000-record limit, now resumes from the next page. Before, it started again from the first page at once, without end, and never read the rest. The next page's URL is kept in the cursor, with any secret in it kept as its `{secret:NAME}` (audit DAP-4).
- `create_target: true` with a `map` makes the table from the map's columns, typed by each column's `as`. Before, it used the kind's default columns and dropped every mapped field but `id` (audit DAP-5).
- CSV and TSV imports: a header that names a field twice, or names no field of the table, is refused (with "is the file comma-separated?"). With `strict=true`, a row with more or fewer values than the header is refused by its line. Without it, such rows are counted in the answer's `ragged_rows` (audit PRF-4).
- Errors answered outside the handlers now have the API's error shape (`error`, `error_code`, `retryable`, `hint`): a body that is not JSON (400), a missing or wrong content type (415), a body over its limit (413, naming the limits and `--max-body-mb`), a route that does not exist (404), a method a route does not take (405), a connection past `--max-connections` (503 `overloaded`), and a write that failed partway on a coordinator, which keeps `partial` and `shards` (audit PRF-6).
- A connection closed because its client held its share (`--max-connections-per-client`) is logged at info, a line a second at most, and counted in `chloedb_connections_closed_past_share_total` (audit PRF-7).
- `timeout_ms`, `snapshot` and the other whole-number fields of a request refuse a value of the wrong type (`"100"`, `-1`, `1e15`) with a 400. Before, they were read as missing. `POST /v1/query` answers `warnings` naming any field it does not know, such as a misspelt `timeout_ms` (audit PRF-8).
- `GET /v1/schema?table=` of a table that does not exist answers 404. `GET /v1/kv/TABLE/KEY` with text for an `INT` key says the key is not a number, rather than `query_invalid`. A vector element past a 32-bit float's range says so, rather than that the vector does not fit its field (audit PRF-9, in part).
- A non-ASCII `Idempotency-Key` header is refused with a sentence saying the header must be visible ASCII, not "is not text" (audit PRF-13).
- A client's own deadline that passes while its query is still being planned counts as the client's (`client_deadline`), as it does once the query runs, and no longer as the server's `timeout`. (A slow runner planned past a 1 ms deadline.)

### Command center

#### Fixed

- Command center: a picture taken for feedback leaves out a new key's secret, a connector's push key and an explorer step's secret; they show as a masked box (CCW-2).
- Command center: table search and saved searches show each row's title and text (the stored source columns of generated fields), not only its id and score (CCB-1, LIV-4, CCB-14).
- Command center: keys of the ops and propose roles see and use what their role allows, buttons say which key a step needs, and a step that needs a reason asks for one and sends it (CCW-1).
- Command center: a tab without a valid key asks for one once and stops polling until signed in; a key the server refuses is not kept; the live button says "Signed out" (CCW-3, CCB-7).
- Command center: the Quality tab asks a few batched statements a few at a time, and caches them for five minutes (160 queries in 1.5 s became 35 in 0.5 s on a 61-field table); a finished backfill no longer counts its rows as missing (LIV-5, PRF-5, CCB-4).
- Command center: a failed import into a new table drops the empty table it made, so the retry works, and a finished import links to searching the table (CCB-2, CCB-15).
- Command center: an apostrophe in a comment no longer breaks running the statement at the caret in the Workbench (CCB-3).
- Command center: Access makes keys for agents (scope, on whose behalf, expiry), and the audit and key lists name an agent's actions as an agent's (CCW-5).
- Command center: Alerts and Home link each alert and problem to the server's own runbook, and Alerts filters the rules that have none (CCW-6).
- Command center: Performance opens the tab its link names, and the address follows the tab shown (CCW-7).
- Command center: the shortcut `g` then `f` opens the Feedback view (CCW-12).
- Command center: on a phone the top bar fits in 390 px and a card's tools wrap below its title (CCW-8, CCB-12).
- Command center: Import reads semicolon CSV with decimal commas, JSON Lines in a `.json` file, prose-named columns as TEXT, and shows a SQLite plan as it is, with ragged rows counted (CCB-5, CCB-8, CCB-9, CCB-10).
- Command center: a coordinator offers Parquet export of its tables rather than steps it cannot run (CCB-6).
- Command center: the demo's test model is labelled "test vectors" and its tables search by words by default, not presented as search by meaning (CCB-11).
- Command center: the palette finds saved searches, and speaks to a single server as one rather than as a cluster (CCB-13).
- Command center: an app's page says it is loading, a failed view load has "Ask again", the list of apps is asked again every minute, and the nav's "!" for an app that stopped answering now shows (CCW-9).
- The website, the command center and the README show the ChloeDB logo.

### Spreadsheets

#### Fixed

- Sheets: the home page can be clicked again. The "Tables of the cluster" card took the grid's scroller class, which is laid over its whole pane, so New workbook, Import and Search could not be clicked (audit DAP-1).
- Sheets: an xlsx file written by anything but Excel (openpyxl, pandas, most exporters) shows its formulas computed when imported. Before, they showed 0 or `#VALUE!` until the next edit (audit DAP-6).
- Sheets: a CSV import keeps `NaN`, `inf` and `Infinity` as text, and ids of more than 15 digits (card and account numbers) as written. Before, the first were dropped without a word and the second lost digits (`4.11111111111111E+15`) (audit DAP-7).
- Sheets: a cluster's table opened live is read to its last row. Past the server's result window (10,000 rows) it reads by the sort's key, by the key or by a number, time or boolean column; where there is nothing to read by, a line above the grid says how far it reads and to narrow it with a WHERE. Before, it went blank past row 10,000 with error toasts (audit DAP-8).
- Sheets: a large import no longer keeps gigabytes. A save makes its rows a write at a time, the open workbooks hold 1.5 million cells together at most (`CHLOEDB_SHEETS_OPEN_CELLS`, least recently used let go first), and freed memory is given back: two imports of a 50,000-row CSV left 204 MB, not 2.8 GB. The import counts its seconds and says about how long a CSV will take (audit DAP-9).
- Sheets: the live table's SQL button opens the workbench on that table as filtered and sorted, its WHERE example is made of the table's columns, and its field select follows dark mode; on a phone the formatting toolbar is one row that scrolls, and the home page's card tools wrap (audit DAP-16, DAP-17, DAP-21).
- Crawler: a crawl that ends says why: its page limit, its depth (with the links it left out, also `beyond_depth` in its status), a full queue, or no links left. A list's next page (`rel="next"`, or a link named "Next") is as deep as the list, so the default depth reads a long list to its end. The form says what the pace means ("about 1 page a second, so 1,000 pages take about 16m 40s"). `chloedb-crawl crawl` now has the pace the docs and the page for site owners state, one request at a time a second apart; it was 2 at once 250 ms apart (audit DAP-10).
- Crawler: the preview reads two levels within its budget, so from a shop's home it reads products and shows their fields (audit DAP-14).
- Feature store demo: a workflow that the deployed model reads can no longer be deleted, which dropped its table and stopped every ranking; deploying a model whose workflows are gone is refused, and the Shop tab says when the shop cannot rank (audit DAP-11).
- Feature store demo: impressions older than three hours are let go (`--keep`), and a model trains on the last 30 minutes, so training stays quick in a demo left running; the shoppers/s badge counts every shopper thread's requests; `serve --local` no longer starts ChloeCtx beside it; the pipeline diagram scrolls on a phone (audit DAP-12, DAP-15, DAP-16, DAP-23).

### Apps

#### Security (audit 4: X-G1, X-G6, X-G7)

- A screen's `PARAMS (x = <expression>)` is worked out in a fresh QuickJS
  interpreter during the build check and screen edits (no Node, 200 ms, 16 MB),
  never with `new Function` in the engine's process. An expression that reaches
  for `process`, `require` or `import()` gives no value and its query is not
  dry-run.
- `claude -p` and `codex exec` start with a reduced environment (PATH, HOME,
  user, locale, temp folder, proxy and certificates, and the CLI's own
  `ANTHROPIC_*`/`CLAUDE_*` or `OPENAI_*`/`CODEX_*`), never the engine's tokens.
  `STRATA_APPS_CHILD_ENV` names more to keep.
- The `codex` provider is off unless `STRATA_APPS_CODEX_ON=1` (or
  `STRATA_APPS_PROVIDER=codex`); a build asking for it otherwise gets a 400.
  When on, its answers cost what their tokens cost
  (`STRATA_APPS_CODEX_USD_PER_MTOK`, default `1.25,0.125,10`) against the
  spending cap, and Stop kills a Codex build at once.
- The MCP server (`engine/mcp.mjs`) signs in with the engine's agent token
  (`STRATA_APPS_AGENT_TOKEN`, or `DATA/agent.token`, which the engine makes on
  first start, mode 0600) and does not start without one. The engine accepts
  it as a studio sign-in beside a cluster too, and refuses a bearer token that
  is neither chloedb-server's nor the agent's, with a login or without.
  MCP `publish` needs `confirm: true` at every access (unpublishing needs none).

#### Changed

- A published app whose owner never chose who may do what is now "Visitors can look" (read only) until the owner picks more in Publish. Before, an app published before the choice existed was open to "Anyone can do anything" without its owner ever choosing it. An owner's explicit choice is kept. After this rolls out, a cluster's apps published before the choice (on production, Kids' reading) become read-only for visitors until their owners choose again (audit APC-9).
- A build whose model writes nothing is given up after 90 s instead of 180 s (`STRATA_APPS_IDLE_S`) (audit APC-6).

#### Added

- A running build can be stopped: Stop beside its progress in the App creator and the studio (`POST /api/apps/<id>/stop`), and "Stop and try again" once it has taken twice as long as builds there usually do. The request ends as "stopped" (audit APC-6).
- The App creator's workspace says when the app engine is not answering (restarting, or down), and reads the page again once it answers (audit APC-8, the workspace part).

#### Fixed

- A network blip, an engine restart or a proxy's 502 while the owner looked at an app no longer starts a model repair, whose result used to go live without the owner choosing it. A call nobody answered is not counted as an error of the app's code: the screen says "No answer from the app just now… Trying again" and reads its data again (audit APC-1).
- An app published as "Visitors can look" no longer shows its visitors forms, Add, Edit, Delete or row actions that write, and ticks are not clickable. A refused call says what was refused ("This app is read-only for visitors…") as a quiet note, and is not recorded as an app error (audit APC-2).
- A visitor of a published app who hits an error sees "Something went wrong on this screen" with Try again, not the internal error with "Ask to fix" (audit APC-5).
- The App creator draws an app at a readable size: with no device chosen, the largest one the stage shows at its real size (computer, tablet or phone), never the desktop shrunk to 17 to 30%. A chosen Desktop is drawn at 60% or more and scrolls sideways. On a phone the device switch stays and the app fills the width (audit APC-3, APC-11).
- An app whose first build failed says "did not build", with Try again and Delete on its gallery card, instead of "building" for ever. A new app is named shortly from its description ("Tiny reading list"), not by its first 40 characters. ChloeCtx's "Create an app" gives the app the title chosen in ChloeCtx (audit APC-4, APC-19, APC-26).
- A failed build says what went wrong in one sentence (the model is not signed in, it is busy or out of quota, it stopped answering, its answer did not pass the checks), with the error under Details. After a failed first build, the composer asks for the app's description again (audit APC-7).
- The App creator home no longer shows a stray "null" after the spending line (audit APC-14), nor the "Ctrl + Enter" hint on a phone (audit APC-11).
- A screen written as JSON gets `:today` as a CREATE SCREEN does, so its first build no longer pays the large model to fix "unbound parameter :today" (audit APC-10).
- A build's estimate counts a round of fixes as its own step ("Fix what the checks found"), and a plan made only of JSON screens no longer logs "Writing 0 pieces at once" (audit APC-13).
- The drawn-page check states contrast to two decimals, against the need for that text size, and text within 0.2 of its need no longer starts a paid change by itself (audit APC-16).
- The app engine docs no longer say that apps' own pages are for admins only while documenting public publishing (audit APC-20).

The prototype app engine (`chloedb-apps/`) also gained declared access per
table enforced by the engine (#505), art direction and computable look
checks (#506), an MCP server for the engine and builds on Codex (#502), and
product photos and a layout for every screen (#511).

### ChloeCtx

#### Changed

- ChloeCtx: the end of a turn no longer waits for query understanding's background search, and no longer answers `decision: block`, which made Claude run again and give its whole answer twice. What the search finds after the turn ends comes with the next prompt. `"stop_recheck": true` in a store's `deep` settings brings the check back as an opt-in: it waits at most 3 s and asks the model to say only what changed, never to repeat its answer. The install page names the model calls ChloeCtx makes per turn and per prompt (audit CTX-1).
- ChloeCtx: the daemon finds the `claude` CLI where Claude Code's hooks found it, then on its own `PATH`, then in the usual install places; `CHLOECTX_CLAUDE` names it outright. `CHLOECTX_DISTILL=0` in Claude Code's environment turns distillation off whichever process started the daemon. `chloectx doctor` shows the CLI the running daemon uses, and fails when it has none (audit CTX-3).
- ChloeCtx: `chloectx prune` leaves alone a store whose project folder went missing in the last week, or is on a drive that is not mounted; `chloectx stores` marks it. `chloectx move OLD NEW` attaches the store of a project that was moved or renamed to its new folder (audit CTX-11).
- ChloeCtx: `chloectx install --project DIR` refuses a folder that does not exist instead of creating it, and in a git repository puts `.claude/settings.local.json` and `.mcp.json`, which name this machine's binary, in `.git/info/exclude`, or says when `.mcp.json` is tracked (audit CTX-9, CTX-12).

#### Added

- ChloeCtx: `chloectx forget ID...` deletes records from a shell, as the `memory_forget` tool does (audit CTX-10).

#### Fixed

- ChloeCtx: a distillation or model call that fails for a reason other than the login keeps the last of the CLI's stderr and its exit status in the error, which the session-start notice and `doctor` show; `doctor` then makes one small model call to say whether it still fails (audit CTX-2).
- ChloeCtx: `chloectx doctor` and `chloectx upgrade --check` say they could not check when the fetch of the checkout's upstream failed, instead of "up to date"; doctor waits at most 3 s for it (it took 21 s on a remote that never answered), and `upgrade --check` exits 1 then (audit CTX-4).
- ChloeCtx: `chloectx blame` no longer says that records a search listed before the edit "are deleted since" when they are still in the store (audit CTX-6).
- ChloeCtx: records whose text quotes a harness tag (`<system-reminder>` in a hook's code, for one) are found again by recall, `memory_ask` and search; only old prompts that were wrapped in such envelopes are passed over, and recalled text shows the tags escaped (`&lt;system-reminder>`), so they do not read as notices from the harness (audit CTX-7).
- ChloeCtx: `chloectx redact --all` (with or without `--dry-run`) goes through the stores one at a time and stops each daemon it started; it left one running per store (audit CTX-8).
- ChloeCtx: `chloectx stop` in a folder with no running daemon says so, instead of starting one and creating a store there (audit CTX-13).
- ChloeCtx: a hook that reaches a daemon as it dies (connection reset, broken pipe) starts a new one and is answered, instead of being dropped (audit CTX-16).
- ChloeCtx: doctor says "1 session", and the new-version notice names `chloectx upgrade` once (audit CTX-15, in part).

#### Inspector

- **One inspector for every memory.** ChloeCtx's page in the command center starts with a picker of every space it can read: the cluster's team spaces, Claude Code projects synced to the cluster, and (with `--local-stores`) this machine's Claude Code stores, a store and its synced copy shown as one project. A space opens on an **Overview** (its rules, decisions, facts, open to-dos, what changed and its latest sessions), then **Ask**, **Memories** (every note, current or with its history), **Sessions** (each one titled by its first prompt and read as a conversation: prompt, tool calls, answer and the notes written down) and **Connect**; a local store adds **Receipts**, **Blame** and **Snapshots and capsules** (cut a capsule from the page). The old Context, Inspector, Receipts and Blame links open the same space's tab ([docs](docs/chloectx/app.md#spaces-and-what-each-shows)).
- **Answers first.** Ask leads with the note that answers the question and what it replaced, then the rules that apply and what else bears on it; the scores behind each (BM25, cosine, section, tokens) and the context as the agent gets it are below, folded. A link carries the question.
- **New calls** for the pages: `GET /v1/spaces/:s/overview`, `/sessions`, `/records`; `GET /v1/memory/overview` and `/v1/memory/notes` for a synced project; and under `/v1/local/:store/` the same views, `POST ask`, capsules (`GET`, `POST`) and `stats`.
- **A demo to show it.** `chloectx/demo/seed.py` plays twenty scripted Claude Code sessions over four weeks on a made-up payments service (refund limits raised twice, a currency added, an incident, a key logged by mistake) through the real hooks and daemon, each turn at its date (`CHLOECTX_CLOCK_OFFSET`), with a scripted distiller and no model call; `--space` writes it into a team space and `--sync` into the cluster's `chloectx_memory`.
- **Fixes.** The synced project's sessions no longer list its entities as a session `-` or fail at 10,000 prompts (`LIMIT 20000`); a note's history in a Claude Code store now finds the notes that replaced it.

No one-way door.

- **See how a space's memory is connected.** Every space in the command center has a **Graph** tab: its notes (by kind; a replaced one hollow), the subjects they are about (sized by how many notes name them), the files they touched, the sessions they came from and which note replaced which. Click a node to light up its neighbours and list them, with its history, its session, *Ask about it* or *Blame it*; search for a subject, file or note; the link carries the selected node. Behind it, `GET /v1/spaces/:s/graph`, `/v1/memory/graph?project=` and `/v1/local/:store/graph` ([docs](docs/chloectx/app.md#spaces-and-what-each-shows)).

No one-way door.

### Network operations

#### Changed

- Network operations: `GET /v1/map/ask` answers with the fixed reader and writes nothing; `POST /v1/map/ask {"q": ...}` keeps the search in `netops_searches` and asks the server's model, at write scope and audited. Opening an incident's board no longer writes its template panels; the first change to the board keeps them. A read key used to cause writes and model spend that left no trace (audit NET-8).

#### Fixed

- Network operations: an outage behind the backbone now reaches the shift. Two built-in rules, `estate-outage` and `estate-degraded`, watch the estate's access nodes, hub routers and power plants that have objects behind them; they are added to an existing store on restart. A down object with 20 or more behind it opens an incident, which the estate's agent works (what the object reported, how far it reaches and which customers, events before it, who is on call) and for which it proposes dispatching field services. Its board reads the hub from the estate's tables. Degraded objects wait on My shift, and Diagnose lists what the estate's rules fire on (audit NET-1).
- Network operations: the map's search lists the words it could not read beside the count, and calls the count an answer to part of the question. An id the estate does not have, a place close to one it has ("Did you mean London?") or a name it cannot place now refuses the question instead of being dropped from it. A place one letter off is read as that place, and the answer says so. A customer is matched by its whole name ("Northwind Energy"). "This week", "this month" and "this year" start on their first day. "Not down" means every other state. In "power plants that failed", power is no longer read as the cause (audit NET-2).
- Network operations: a pasted carrier notice's window is read from its Start and End lines (or its Window line), in the time zone it gives: CET, BST, EST, offsets, AM and PM. The page shows what was read before the notice is kept, and refuses a zone it does not know, an end before the start, or a window over two weeks (audit NET-3).
- Network operations: an estate object's id, pasted into the search with Enter, opens that object on the map. It used to say "Nothing in the network is called that". The lookup also finds objects by the start of their id, and business-park customers by name (audit NET-4).
- Network operations: an idle map tab no longer reloads its window every 10 seconds. It reloads only when something in view changed, and at the latest every five minutes (audit NET-6).
- Network operations: approvals, acknowledgements, silences, notes, change steps and panels are recorded under the key that made them (`x-chloedb-principal`), no longer as "You" (audit NET-9).
- Network operations: an alert rule's comparator and threshold, as saved in the rule editor, now decide which rows fire. "Run it now" shows which rows would fire (audit NET-10).
- Network operations: the place in view on the map no longer narrows searches made on other pages (audit NET-15).

### Documentation

#### Added

- `scripts/check-doc-urls.py` and the workflow Docs URLs check that what the docs tell people to install exists: every pinned release download, the archives `install.sh` fetches for the server and each `--with` program on every platform, the ghcr.io image tags, a Homebrew tap, and PyPI's chloedb at the latest release `docs/site/versions.json` names. It runs on a push to main that changes the docs, after each release, every night (a failure opens an issue) and by hand. It would have caught the docs pinning 0.3.2 before its release, `--with crawl|sheets|ctx` on a release without those archives, and the missing Homebrew tap (audit ONB-1, ONB-2, ONB-4).
- `chloedb-server --help` lists its options by topic (Start, Database, Merges and storage tiers, Replication, Cluster, Security, Limits and timeouts, Agent and models, Logs and alerts, Apps and tenants), and `-h` lists only the ten a first run needs (audit ONB-9).

#### Changed

- The website's "Get started" installs the prebuilt Python package (`pip install chloedb`, `python3`) and offers the server's installer, with the source build as the alternative where there is no wheel (audit ONB-3).
- The quickstart no longer offers `brew install chloetech-ai/tap/chloedb`: the tap does not exist yet (audit ONB-4).
- The server reference shows the current command center (Start here, Tables, the SQL workbench, Schema, Logs, the query builder, the graph explorer) in WebP, and its navigation as it is now; every docs page loads its pictures lazily. server.html loads 0.75 MB instead of 7.7 MB (audit ONB-10).
- `--schema` takes the JSON itself as well as a file, for the server and for `import` (audit ONB-8).
- The PyPI placeholder, which pip picks only where ChloeDB has no wheel, says so and how to build from source (audit ONB-6, the text; the registries' old 0.0.1 placeholders change only when a person runs reserve-names.yml and yanks PyPI's 0.0.1).

#### Fixed

- `chloedb-server admin ... | head` and every other command's answer into a closed pipe end quietly with exit code 0, instead of panicking with 101. An answer that is not JSON shows its HTTP status (`{"status": 404, "error": "HTTP 404 Not Found", ...}`) (audit ONB-7).
- `chloedb-server fetch-model` keeps the files already there that match their pinned SHA-256, instead of downloading 130 MB again (audit ONB-15).
- `chloedb-server doctor` suggests `pip install --upgrade chloedb` from PyPI, not a GitHub release named after its own version, which a build from main is ahead of (audit ONB-19).
- `--help`'s link to the connectors page, the installer's `--help` under `curl ... | sh` (it printed nothing), the release notes' `--db my.strata`, the quickstart's "a database is one file", and the apps guide's output when the crawler is installed beside the server (`--start-apps off`) (audit ONB-12, ONB-14, ONB-18, ONB-20, ONB-21).

### Operations

#### Added

- The Kubernetes operator's `spec.coordinatorStorage` (`{size: 2Gi, storageClassName}`), optional: each coordinator gets a volume of its own and keeps its log store (events, alert history, query statistics, table growth), audit log and alert rules there (`--log-store /data/logs`, `--audit-log /data/audit.jsonl`, `--alerts /data/alerts.json`, unless `coordinatorArgs` gives them), so a rollout no longer forgets who changed what. With it the coordinators run as a StatefulSet; without it, as a Deployment, unchanged. Giving it starts the new coordinators beside the old ones, which stop once the new ones serve. Run two coordinators with it, since a StatefulSet restarts one before it starts the next. Home's readiness card now names this field instead of `spec.auditLog`, which never existed (audit LIV-2).

#### Fixed

- The built-in alert rules no longer fire on a calm cluster (audit LIV-3). Before this, they fired 289 times in 24 hours on production for nothing:
  - "Query p99 doubled" (and any `vs: before` rule over a latency) treats a window under 20 ms as unchanged.
  - The 5xx, query-error and availability shares are judged only over 100 requests or more, or 10 failures or more.
  - "A replica is behind" measures how long the replica has lacked a transaction its primary committed, from the primary's numbers over the series. It used to divide transactions behind by the write rate, which read as a day behind whenever writes paused. It also leaves out a replica being sent a whole copy, or one not heard from since the primary started.
- A coordinator's Alerts page and `GET /v1/alerts` (`copies_firing`) now show the alerts that fire on the copies' own rules, which only the copies' log lines showed before (audit LIV-3).
- A shard's copies are kept on different nodes more reliably. The operator weighs a shard's own copies twice as much as the third copy a rollout adds for a while, which used to leave both copies of every shard on one node once that third copy went. The rule is still preferred rather than required, so a cluster with fewer nodes than copies still runs. Home's Problems card and readiness list now say when every copy of a shard runs on one node while the cluster has other nodes (audit LIV-6).
- A server removes, when it starts, the copies of its database that a transfer left behind because the server was stopped partway through (`.db.strata.copy-*`, `.db.strata.restore-*`). Home now reports a copy that uses over twice the disk of its shard's other copies, when the difference is more than 1 GB (audit LIV-7).
- Home's readiness card counted routes twice ("3 routes" for 2) and counted built-in rules as having no runbook. It also said "1 shards have one copy". The full `GET /v1/alerts` now gives `runbook_docs` to each rule whose runbook is compiled in (audit CCW-13).
- Rollouts: the upgrader's note about the app engine's image now starts with "App engine image:", so it no longer reads as the servers' version (audit LIV-15).
- The cluster simulator's check of a late-won term now asks that its primary hold the writes acknowledged by an older term's primary, as Raft promises, rather than every write that was a transaction of an older term. A retry of an old term's write is acknowledged by a newer primary once that primary's term commits, so nightly seed 624144435 reported a lost write that was none: a cut-off replica won a stale term after a 10 s vote round and was fenced before serving anything. The server is unchanged (0.3.2 fails the seed the same way); the seed is kept in the simulator's test.
- CI: the on-demand runner's secrets (`DIGITALOCEAN_TOKEN`, and `CI_RUNNER_ADMIN_TOKEN`, which has Administration on the repository) are read from the environment `ci-runner`, which only main may use, so a branch's copy of `ci.yml` cannot read them. A run outside main (a pull request's, the merge queue's, a tag's) is placed by `ci-runner.yml`'s new job `place`, which runs main's code with them and posts the label as the commit status `ci-runner`. The secrets have to be moved from the repository into that environment (`.github/ci-runner/README.md`) (audit 4, X-I2).
- CI: chloedb-server's Postgres tests fail instead of passing without running when the runner has no Postgres (`CHLOEDB_TEST_POSTGRES_REQUIRED`, set on Linux), and the on-demand runner's image has Postgres's programs again, unpacked so that no service starts while it is built (audit 4, X-I3).

## 0.3.2

What the persona swarm found, agent-friendly administration, server limits
and understanding through a coordinator: `chloedb-server-v0.3.2`,
`chloedb-py-v0.3.2` and `chloedb-js-v0.3.2` (the Java package is built
from source; `chloedb-crawl`, `chloedb-sheets` and `chloectx` are now
attached prebuilt to the server's release, and the installer takes
`--with crawl,sheets,ctx`). The file format stays 3. The shard protocol
moves to 21: write receipts' counts, strict writes and
`insert_if_absent` are additions a coordinator uses only with shards of
21, and an older shard's counts read `null`. One plan feature is new,
`full_join` (`FULL JOIN`), which a 0.3.1 shard refuses by name, so
upgrade the shards before the coordinators. Auth changes: keys gain the
`propose` and `ops` scopes and agent marks (#482); keys limited to tables
reach more routes (#457).

### What changes by default

- A duplicate primary key answers 409 over HTTP (it was 400), and an
  `INSERT` into a `shard_by` table refuses a key another shard holds.
- A `set`, `add` or `delete` with the wrong `routing` is a 404 instead of
  `200 written: 0`; `"if_missing": "skip"` keeps the old behaviour.
- `written` counts a key repeated in one batch once.
- An agent's `DROP TABLE` or `DELETE` without `WHERE` waits for a person's
  approval, even with an admin key.
- New installs keep models in `~/.local/share/chloedb/models`; a machine
  with the old `~/.local/share/strata/models` keeps using it.
- `chloedb-crawl` and `chloedb-sheets` take `--server URL`; `--strata`
  stays an alias.
- A published app's visitors no longer start a repair, and apps published
  before keep the "change" access.

### Agents and administration

#### Server

- **Keys for agents.** Two new scopes for agents' keys: `propose` (reads, the server's views, plans, dry runs, change requests) and `ops` (that, and the bounded actions alone: contain rules, cancelling queries, silencing and acknowledging alerts, notes). Keys can be marked as an agent's (`"agent": true`), last minutes (`expires_minutes`), and name the person they act for (`on_behalf_of`). `GET /v1/whoami` answers `may`: what the key may do alone, whether it may propose or approve changes ([docs](docs/chloedb/server.md#agents-and-administration)).
- **Error codes.** Every error answer carries a stable `error_code`, `retryable`, a `hint` and a `docs` link beside its sentence; over gRPC the code is in the metadata `chloedb-error-code`.
- **Tiers, reasons and caps.** Every route has a tier (0 reads, 1 bounded, 2 changes that can be undone, 3 losses or costs). An agent's change says why (`x-chloedb-reason`), its contain rules last an hour at most and its silences two hours, and the audit log names agents (`actor: "agent"`, with model, session and run from `x-chloedb-agent-*`) and the person they act for.
- **Dry runs and change requests.** `POST /v1/admin/changes/preview` dry runs a change of any route in one shape (tier, what it would do, undo, blast radius, rows a SQL statement touches, the route's own plan, a fingerprint). `POST /v1/admin/changes` files it for a person to approve; an approval by an agent is refused, and an approval of a plan the cluster has since moved away from fails `plan_stale` instead of being applied. An agent's `DROP TABLE` or `DELETE` without `WHERE` is refused, even with an admin key, until a person approves it. The command center's On-call page lists change requests beside the agent's proposals.
- **Operations over MCP.** A role or admin key sees operations tools in `/v1/mcp`: health, cluster, load, alerts, events, queries, logs, metrics, audit, the catalog, jobs, runbooks, plans, dry runs, change requests, and the tier 1 actions.
- **The catalog, jobs, waiting, runbooks.** `GET /v1/catalog` lists every route with its scope, tier and MCP tool, and every error code; `GET /v1/admin/jobs` lists what runs in the background in one shape; `GET /v1/events?after=&wait_s=` waits for the next event; runbooks for every built-in alert and fleet problem ([docs](docs/chloedb/runbooks.md)), found with `GET /v1/runbooks?for=` and named in each fleet problem's `next`.
- **`chloedb-server admin`.** The API from a shell, one request at a time, JSON out, with exit codes by outcome (0 done, 2 refused for the key, 3 stale, 4 retry later, 5 not reached).

One-way door: auth (new key scopes and roles, approval rules). Role keys are stored as read keys with a role beside them, so a server from before roles admits them as read keys.

(#482)

### Server limits and operator rights

#### Changed

- chloedb-server's listeners hold each client to more (audit N-8, D-B7): one client address may hold half of a listener's connections (`--max-connections-per-client`); past the cap, 8 connections are still taken for `/livez`, `/readyz` and `/v1/health` (two at once per address) and any other request on them is answered 503 with `Retry-After`, so a full server is not restarted for failing its probes; a connection with no request under way for `--idle-timeout-secs` (300) is closed, over HTTP/2 too; a request body that pauses for `--body-timeout-secs` (30), or averages under 1 KiB a second once it has taken that long, fails; a write the client does not read for `--write-timeout-secs` (60) closes its connection. Each is 0 for no limit.
- gRPC is served on the same accept loop as HTTP, rather than by tonic's server: the connection cap and share per client, the TLS handshake, idle and write timeouts and `--stop-grace-ms` now hold on the port between coordinators and shards too, a follower's gRPC `Feed` ends when the server stops (the rest of D-B6), and `grpc-timeout` is honoured as before. A gRPC connection carries 1,024 calls at once; past that a client waits for one to end.
- A budget of model calls per API key (audit D-B13): `--model-calls-per-key-day` (10,000) calls a day (UTC) with the server's own credentials, counted where each request is sent: `/v1/agent` and the on-call agent a call a step, `/v1/judge` a call a row, `llm()` a call a question asked (answers from memory are free). Past it the call answers 429 before anything is sent. The `--token`, the cluster's token and callers that bring their own `api_key` are not counted. `GET /v1/admin/model-budget` shows today's counts. Hosted embedding models are not counted yet. `chloedb::query::llm::Split::finish_within` takes a caller's budget.

#### Fixed

- A key limited to tables refused on the gRPC port is written to the audit log, as an HTTP refusal is (audit N-D11).
- `POST /v1/admin/models/fetch` answers 409 on a server started with `--no-fetch-models`, rather than downloading what it was told not to (audit N-D12).
- The Kubernetes operator rolls an app's pod when it makes the app a new key: the key's id is an annotation of the pod template, so a pod no longer keeps a revoked key until it restarts. It makes the new key before revoking the old one (audit N-D4).
- The operator may only read Secrets cluster-wide; it creates, patches and deletes them in namespace `strata` through the new ClusterRole `strata-operator-secrets` and a RoleBinding there. A cluster in another namespace needs that ClusterRole bound there too (audit N-D9).
- A coordinator judging a key limited to tables learns the cluster's tables and stored functions again (at most once a second) when the statement names a table it does not know or calls a stored function, so a table or function another coordinator just made or changed is judged as it is now (audit N-D5).

(#478)

### Cluster

- **A primary key is unique across shards for `INSERT` into a `shard_by` table.** An insert whose `shard_by` value placed it on another shard than an existing row with the same key used to succeed, leaving two rows with one key. The coordinator now asks every shard's primary for the inserted keys first (one read by key per shard per request) and refuses a key another shard holds (409, naming the shard). Inserts of one key through one coordinator are serialized; two coordinators inserting the same key at the same moment are not (documented).
- **A `set`, `add` or `delete` with the wrong `routing` is an error**, 404 naming the routing and the column, instead of `200 written: 0`. `"if_missing": "skip"` keeps the old behaviour.
- **Write receipts.** `/v1/write`, `/v1/kv` and `/v1/import` answer `receipt: {received, inserted, replaced, updated, deleted, collapsed, not_found, per_shard}`. `written` no longer counts a key repeated in one batch twice (an import of `[{"id":30},{"id":30}]` answers `written: 1`, `collapsed: 1`). `strict=true` (body of `/v1/write`, query of `/v1/import`) refuses a write that would replace a row, repeat a key or miss a key, writing nothing.
- **`INSERT ... ON CONFLICT DO NOTHING` through a coordinator** (any rows), and **`DO UPDATE`** when its rows go to one shard.
- **`UPDATE ... LIMIT` and `DELETE ... [ORDER BY] LIMIT` through a coordinator**: the rows are picked by key first, then changed if they still match.
- **`GET /v1/export?format=jsonl|csv|arrow|parquet&tables=T`** works on a cluster (read by a scroll over the shards) and for a key with read on the table; JSON lines, CSV and Arrow work on one server too. SQLite export stays the admin's.
- **Rank-log export and interleaving credit run through a coordinator** (`/v1/rank_log/export`, `/v1/rank_log/interleaving`), over every shard.
- **`POST /v1/sql/check`** says whether a statement will run on this deployment (single server or cluster) without running it.
- A SQLite or Parquet import through a coordinator creates a missing table on every shard, as its `plan=true` said it would.
- `ALTER TABLE ... ADD COLUMN ... NOT NULL` without a default counts the rows of every shard that refused it, not the first shard's.
- A 409 for `if_seq_no` names the key as SQL (`2`, `'p1'`), not `Int(2)`.
- A duplicate primary key answers 409 over HTTP (it was 400); gRPC is unchanged.
- `/v1/import` takes `empty=default|null|text` for CSV and TSV empty cells.
- New docs page: `docs/chloedb/cluster-limits.md`, what a cluster refuses or does differently.
- Shard protocol 21 (backward compatible): `WriteResponse` carries the receipt counts; shards take an operation's `if_missing` and `strict`, and the `insert_if_absent` operation.

(#457)

### Engine and SQL

- **`FILTER (WHERE ...)` on aggregates works.** It was silently ignored outside windows (`COUNT(*) FILTER (WHERE severity = 'error')` gave the total). Any aggregate takes it now, grouped or not, in `SELECT`, `HAVING` and `ORDER BY`, over one table, joins and derived tables; `FILTER` on a non-aggregate is an error.
- **Statistical aggregates**: `STDDEV`/`STDDEV_SAMP`, `STDDEV_POP`, `VARIANCE`/`VAR_SAMP`, `VAR_POP`, `COVAR_SAMP`, `COVAR_POP` and `CORR`, with PostgreSQL's NULL rules. They are computed from sums and counts, so they merge over shards.
- **`COUNT`, `COUNT(DISTINCT)`, `MIN` and `MAX` of a TEXT column** (or of an unindexed one) count its stored values instead of being refused.
- **Intervals and time arithmetic.** `INTERVAL '15 minutes'` (and `'1 day 12 hours'`, `INTERVAL '15' MINUTE`, months and years on the calendar). `now() - INTERVAL '2 hours'`, `TIMESTAMP '...' + INTERVAL ...` and `date_trunc('day', now())` are values in `WHERE`, so the index range still serves them. A column plus an interval is computed per row. `now() - 3600` is now an error that points to `INTERVAL`, because the number could be seconds or microseconds; before, it silently subtracted microseconds.
- **`DATE`** is accepted as a column type. It is an alias of `TIMESTAMP` (a date written alone is its midnight UTC), with no change to the file format.
- **`RIGHT JOIN`** as the first join of a `FROM` (run as the swapped `LEFT JOIN`; `*` keeps the written order).
- **Correlated `EXISTS` / `NOT EXISTS`** with one equality to the outer row run as a semi or anti join: `IN` / `NOT IN` of the subquery, with `NULL`s handled as `EXISTS` handles them.
- **Explain hits**: `chloedb::query::sql::ranks` is true only for a SELECT whose ORDER BY ranks rows (a column or a ranking), not for grouped, aggregated or computed orders. `/v1/explain` with `hits` no longer has to fail on `GROUP BY ... ORDER BY revenue`.
- **CSV import**:
  - Error line numbers are right with CRLF line endings (they were one short).
  - An empty cell takes the column's `DEFAULT` (so `NOT NULL DEFAULT` columns accept gaps). `Rows::empty_cells(EmptyCell::Null | Text)` chooses NULL, or the empty string for text.
- **INT columns accept whole floats** (`2.0`, as pandas writes them) from SQL, JSON and CSV. `2.5` is refused with a clear message.
- **Messages**:
  - A key repeated within one INSERT (or one file or transaction) says so, instead of "already has a row".
  - Keys print as SQL values (`2`, `'a'`), not `Int(2)`.
  - EXPLAIN shows `price > 10`, not `price >= 10.000000000000002`.
  - `Error::code()` gives stable codes, `conflict` for a duplicate primary key. `Error::message()` and `Error::within()` add context without repeating the `schema error:` prefix.
- **`PACK n TOKENS`** no longer marks a row with several text columns "skip: over budget" while half the budget is left. The row is excerpted. Ranking is unchanged.

(#457)

### Keys limited to tables

- **Query parameters read the queried table.** `{"embed": ...}` and `{"vector_of": key}` without a `table` resolve against the table the statement queries (refused, saying to add `"table"`, when it reads two tables with such a field), not the server's first table. A key limited to tables needs read on whatever table a parameter uses (a row's vector, a field's model and its provider key): it can no longer resolve a parameter against another table. `/v1/embed` uses the model of a table the key reads (the first such with none named), and `/v1/models` lists only the fields of tables it reads. The engine's "is text" error suggests `{"embed"}` with the table and field, and for a field with no model suggests a vector or `{"vector_of"}` instead.
- **`/v1/schema` with no table** answers the first table a limited key may read, never a 403 about a table it did not name (Python's `table()`/`search()` and JS's `search(..., {table})` with such keys).
- **`PACK n TOKENS`** works for limited keys (it was a syntax error).
- **More routes for limited keys, each checked by its tables:** `/v1/plan` (write on what it writes, read on every table it reads, subqueries and edge tables too; Python's `next_items`), `/v1/schema/check`, logged queries (`"log"`, into a table the key writes; admin there to create it), `/v1/rank_log/export` and `/interleaving`, OTLP `POST /v1/logs` into a table the key writes, `GET /v1/apps` and `/v1/apps/<name>/...`.
- **Apps that check their callers.** An app whose `app.json` says `"checks_callers": true` gets limited keys (with `x-chloedb-tables`) and changes from write-scope keys; other apps keep needing a key of every table (admin to change). ChloeCtx now checks its callers: read on `ctx_<space>` to read or ask, write to add or retire notes (so a write key, or `ctx_ops_*=write`, records notes), admin to create a space; builds, the on-call handoff and local stores stay admin-only. Changes through apps are still audited.
- **MCP** refuses a limited key with a JSON-RPC error (`-32001`, HTTP 403) that says why.
- **Shard addresses** are left out of query steps (`shard 0 query`) for any key that is not the server's admin.
- **Rank profiles take one text parameter for both BM25 and a vector** (`bm25(title, :q) + similarity(emb, :q)`, and `embed(:q)` in a profile): the profile is written out and its texts embedded, the plan still naming the profile. Existing queries rank as before.
- **Explain with `hits`** no longer fails on queries that order groups or computed columns: it explains without hits and says why (`hits_skipped`).
- **Presets.** `POST /v1/admin/keys {"name", "preset": "analyst" | "oncall" | "ml-app" | "app-owner", "prefix": "team_*"}` makes a job's key, stored as the grants and allowance it expands to. Limited keys may also be given `"routes": ["fleet"]` (GET of fleet health, events, metrics and charts) and `"apps": [...]` (apps that do not check callers, readable through with GET when the key reads every table). `/v1/whoami` and the key list show `limited`, `preset` and `allow`; whoami lists the presets.
- **Command center with a limited key:** the header shows the key's preset or grants instead of "admin"; Home says the key can't see the fleet or the timeline instead of spinning or saying nothing happened; a route refused to a limited key is not polled again; the Apps page says the listing was refused, or that an app takes a key of every table, instead of "This server has no apps"; the key page offers presets and shows each key's limits; rotating a limited key in the UI keeps its limits (it used to make a successor of every table). Results show INT ids, years and codes without thousands separators.
- docs/chloedb/server.md no longer says keys per table are "not there yet", and lists every route a limited key may use.

(#457)

### Logs

- **Log explorer** in the command center (Operate > Log explorer, `g e`, `#/explorer`): the log lines in the database's own tables, any table with a `TIMESTAMP` and a text column. A search box (words and `"phrases"` as `ILIKE '%...%'` on a column with a substring index or `MATCH` without one; `host:core-rtr-02`, `attributes.iface:Gi0/1`, `code>=4`, `level>=warn` by the levels the table holds, `-term`, or `WHERE ...` as written), the last 15 minutes, hour, day or week (`ts > now() - INTERVAL '15 minutes'`) or a custom window, a count of lines per minute (wider buckets over longer ranges) with errors and warnings apart and a click to zoom, the newest lines followed live every 3 s with a pause, the lines before and after one on the same host (±10 to ±200), the lines grouped by pattern (numbers, IPs, MACs, hex ids, times and quoted text as placeholders, syslog mnemonics kept), and every statement it ran, to copy or open in the SQL workbench. It works with a key limited to tables (the `oncall` preset's `logs_*`), showing only the tables the key reads; against a server from before `INTERVAL` and `time_bucket` it falls back to literal times and `date_trunc`. Without a log table it shows **Send your logs here**: the table to make, an `oncall` key, an OpenTelemetry Collector's settings for this server's `/v1/logs` and a JSON lines import, with buttons that make the table and the key for an admin.
- **`time_bucket(width, t)`**: the start of a bucket of a fixed width of a `TIMESTAMP` (`time_bucket('5 minutes', ts)`, or `INTERVAL '5 minutes'`), buckets counted from 1970-01-01 UTC; grouped from the column by the executor, as `date_trunc` is, on one server and through a coordinator. A plan that uses it needs the new plan feature `time_bucket` (protobuf `FUNC_TIME_BUCKET = 23`), which older shards refuse by name.
- **`POST /v1/import` into a table without a primary key** (a log table) inserts its rows; it failed with "table ... has no primary key" before.
- **Docs**: a new guide page, [Search your logs](docs/guide/logs.md): a log table, an `oncall` key for `logs_*`, an OpenTelemetry Collector's settings, OTLP JSON and JSON lines with curl, and the first five queries of an incident (the last 15 minutes' errors, per host per minute, an interface flapping, an IP, the top kinds of line), run by `docs/guide/run.py`.

(#457)

### Recommendations

#### Added

- **`FULL [OUTER] JOIN`.** The rows of a `LEFT JOIN`, then the right side's rows that matched none, with `NULL`s on the left. `NULL` keys match nothing, extra `ON` conditions decide which pairs join, and `WHERE` filters the joined rows (neither side's scan). It runs on the operators (DataFusion falls back to them) and through a coordinator, which joins the shards' rows as one database does. A plan with one needs the plan feature `full_join`; gRPC's `JoinKind` gains `JOIN_KIND_FULL = 2`. ([query language](docs/chloedb/query-language.md#subqueries-with-and-joins))
- **Recommenders: `CREATE RECOMMENDER` and `recommend()`.** `CREATE RECOMMENDER shop ON events(usr, item, ts) [ITEMS items(emb)] [OPTIONS (window = 3, history = 20)]` counts the events already written into the table `shop_counts` (next-item counts, items seen together within `window` events, item popularity, each user's last `history` events), and every transaction that inserts events updates the counts in the same transaction, at commit, so replicas get them with the events. `ORDER BY recommend('shop', :u) DESC` ranks a table's rows for a user by reciprocal rank fusion of what came next after their last items, what was seen with their recent items, the items nearest those by `maxsim` (with a vector field) and popularity, leaving out what the user had; it is `ranked(...)` of the keys, so it filters, joins and fuses as any score. `SELECT * FROM recommend('shop', :u, k => 10)` lists the items with each list's rank. `DROP RECOMMENDER` drops it. The definition is a stored function, so no file format changes. A coordinator refuses both, and a key limited to tables needs read on the counts table. ([query language](docs/chloedb/query-language.md#recommenders-create-recommender-and-recommend), [cluster limits](docs/chloedb/cluster-limits.md))

#### Changed

- `FULL JOIN` is no longer refused; `CROSS JOIN` still is, with what to write instead.
- next-items.md says that a `ppr` walk is refused through a coordinator, and points to `CREATE RECOMMENDER`.

(#457)

### Ops, apps and the workbench

#### chloedb-server
- **Alerts.** On a server without a log store, a backtest no longer fails with 409 by default. It covers the points memory holds, and its `note` says what it covered instead of what was asked. `from` and `to` take milliseconds or seconds (as numbers or digits) or ISO 8601 text; anything else is a 400. A saved rule can be backtested by its id (`"rule": "p99-jump"`). A relative rule's title gives the ratio: "p99 2.4x its baseline (> 2x, mean over 5m against the 5m before)", not "Query p99 doubled".
- **Fleet.** A filling disk's headline and its problem give the same estimate in the same unit ("fills in about 3 h at the last hour's rate"). The headline used to say "within 3 days".
- **Apps that check their callers can act as them.** An app's token sent with `x-chloedb-as-tables: <grants>` is narrowed to those grants, each capped at the token's own scope, and its statements are checked as the caller's key would be, routes included. The header only narrows: a key already limited to tables gets 403, and a narrowed credential is never the root. Sheets, the crawler and the app creator now declare `checks_callers`. chloedb-server passes an app's `content-disposition` and `x-app-*` response headers through.
- **Presets.** The analyst preset makes its own workbooks in Sheets. The app-owner preset (`app_*` admin) builds, changes and publishes apps in the app creator.
- **SQL workbench.**
  - An answer larger than a query may return (10,000 rows) is no longer refused. The workbench shows the first 10,000 rows, read as a scroll, under a "Showing 10,000 of N rows" banner.
  - **Export all** writes every row as a CSV, a page at a time. A whole table (`SELECT * FROM t`) comes as Parquet from `/v1/export` when the key may export, and as a CSV otherwise.
  - A new **Health** tab profiles each column by SQL over the whole answer: NULLs, distinct values, min, max and spread. It flags columns that are always NULL, columns with one value, and sums that a few rows make.
  - One number format is used for the grid, charts and Sheets, and chart axes get round ticks (0, 500M, 1B, 1.5B rather than 991.7M).

#### ChloeCtx
- The owner of a space, or an admin, can delete it (`DELETE /v1/spaces/:s`). This drops its `ctx_<space>` table and its `ctx_spaces` row.
- A note or an incident can carry its own date: `when` (or `ts`), in seconds or ISO 8601 text.
- A team can keep its own incidents in a space of kind `incidents`. Seen before reads every incidents space the key may read. `POST /v1/incidents/match {lines}` matches pasted log lines to past incidents, as an alert is matched.

#### Sheets
- Numbers show at most 15 significant digits, the way Excel's General format shows them, in the grid, CSV and xlsx (226159459.83, not 226159459.829999998).
- An xlsx export says how many charts it left out (`x-app-left-out`, and a warning in the app).
- The home page no longer clips its right column.
- Sheets checks its callers. Workbooks have an owner, the list offers **Mine / Shared / All**, and **Share** opens a workbook to other keys. Query blocks, live tables and copies to tables run as the caller's key.

#### Crawler
- A field found both in JSON-LD and in another source of a page is one column, not two (formula1_com's duplicate event columns).
- The crawler checks its callers. A crawl is its starter's: only its starter, or an admin of every table, can steer it. A key limited to tables lists only its own crawls, and a crawl writes as the key that started it.

#### App creator
- The engine checks its callers. A key limited to tables reads an app with read on `app_<id>_*`, uses it with write there and changes it with admin there. Building a new app takes admin on `app_*`.

(#457)

- **Security: visitors of a published app can no longer start a paid repair.** `POST /api/apps/<id>/error` from the public (and errors a visitor's call or trigger hits) is only recorded under Usage; it never reaches the forge or the model's prompt. Before, anyone could spend the model budget and put their own words into the code generator's prompt for a live app.
- **Privacy: a published app's people are for its members.** `GET /api/apps/<id>/people` answers only signed-in members of that app (names; emails for its owner). A session from another app on the same address no longer makes a visitor a member, and signing up with an email that has an account signs in with the right password and otherwise says only that the email and password cannot be used.
- **Who can see this?** Publish offers four choices: only me (the default), visitors can look, customers can look and add (and change only what they added themselves), anyone can do anything (typed confirmation). Apps published before keep what they had (anyone can change). **See it as a stranger** opens the app with no account under the choice being weighed, and the tab warns of columns that look like emails, phone numbers or addresses.
- **Visitors' writes are checked on the server**: a column whose forms offer fixed choices takes only those, a field is at most 10 KB (`STRATA_APPS_PUBLIC_FIELD_MAX`), a row 64 KB (`STRATA_APPS_PUBLIC_ROW_MAX`), a list 100 items; refusals are clear 400s. Failures no longer tell visitors shard names or physical tables.
- **Owner basics**: rename, duplicate (with rows), export (JSON, and CSV per table) and delete an app, which drops its own `app_<id>_*` tables and nothing of other apps. A name the owner gives stays, and a label that starts the description (`acme-shop ...`) stays in front of the name the model picks. The bar and the gallery show what each app's builds cost.
- The build's checks no longer show up in Usage as a visitor's insert; suggestions from before a new version are retired when it goes live; the progress estimate is scaled to the real duration of past builds (and starts at about a minute, not 24 s).
- The app creator's Desktop preview draws the app 1280 px wide, scaled to fit; Data is the first tab; the Publish panel repaints after a change; its switches are labelled; a stray "null" is gone from the home page and the empty stage.

(#457)

### Packaging, releases and docs

#### Added

- **Prebuilt crawler, Sheets and ChloeCtx.** A `chloedb-server-v<version>` release also carries `chloedb-crawl`, `chloedb-sheets` and `chloectx` archives for each platform (each with its `.sha256`), and the website's installer installs them with the server: `curl -fsSL https://chloetech-ai.github.io/chloedb/install.sh | sh -s -- --with crawl,sheets,ctx` (or `CHLOEDB_WITH`). The crawl, apps and ChloeCtx setup pages use them, and keep the build from source as a fallback.
- **npm packages, napi-rs style.** `js-release.yml` packs `chloedb` (the JavaScript, with a package per platform as optional dependencies) and `@chloedb/<platform>` (the addon), tests the pair in an empty app, and publishes them on a tag once the repository variable `CHLOEDB_PUBLISH_NPM` is `true` (environment `npm`, secret `NPM_TOKEN`). `index.js` loads the addon from the platform's package when none is beside it. `scripts/pack.mjs --npm <platform>` and `--npm-main` pack them by hand.
- **`chloedb-server doctor`.** Checks a machine for the guides, changing nothing: the version against the latest release, whether the Python package is ChloeDB or PyPI's placeholder, the models folder, port 7700, and glibc against the prebuilt programs. `ok`/`warn`/`FAIL` lines with a fix each; exits 1 on a `FAIL`.
- **Python:** `chloedb.AsyncClient` and `AsyncSession` (asyncio; the client's methods as coroutines, run in the loop's executor, standard library only), `chloedb.__version__`, `py.typed`, and `search(text, table=...)` on `Database` and `Client`.
- **Python:** `chloedb.DuplicateKeyError`, raised for an `INSERT` of a key the table holds, with `.table`, `.field`, `.key` and `.row` where the message names them. It is a `ConflictError` (code `conflict`) and still a `BadRequestError`.
- **Docs:** each page of the site says which version it describes and the latest release (from `docs/site/versions.json`), `<!-- since X -->` markers show as "since X (unreleased)" badges, `build_docs.py --snapshot TAG` renders a release's docs into `docs/site/docs/v<version>/`, and `server-release.yml` opens a pull request with that snapshot on a server tag.
- **CI:** `scripts/check-old-names.py` fails on Strata or Engram on the pages a newcomer follows, the installers and the placeholders (except on lines that say they are the old names); `scripts/check-documented-python.py` fails on a `chloedb.X` module or `chloedb[extra]` the docs name that does not exist, or that the latest release lacks without a since marker.

#### Changed

- **PyPI, npm and crates.io placeholders 0.0.2** say they are placeholders for ChloeDB and give the install that works (`pip install --upgrade chloedb -f .../chloedb-py-v0.2.0`), not Strata's `strata-db 0.1.0`. `reserve-names.yml` republishes them.
- **The quickstart and the Python page** no longer tell readers to run a bare `pip install chloedb` (which installs the placeholder): `pip install --upgrade chloedb -f <release>`, and plain `pip install chloedb` once PyPI holds the releases. The Linux wheels' glibc is stated as released: 0.2.0's are `manylinux_2_35`; from 0.2.1, `manylinux_2_28`, which `py-release.yml` now checks.
- **The models folder** is `~/.local/share/chloedb/models` for new installs (`chloedb::embed::models_dir`, chloedb-py, the docs); a machine with `~/.local/share/strata/models` and no `~/.local/share/chloedb` keeps using the old one.
- **`--server URL`** names the server for `chloedb-crawl` and `chloedb-sheets`, as for `chloectx app`; `--strata` still works.
- **`{"vector_of": key}` and `{"embed": text}` parameters** in the Python and JavaScript bindings read the table the statement's `FROM` names, not the database's first table, unless they name one with `"table"`.
- **Python and JavaScript clients:** a call that names a table (`search(..., table=)`, `table(name)`, `schema(table)` in JS) reads `/v1/schema?table=` for that table alone and never the server's first table, so a key limited to other tables works with them.
- **The ChloeCtx demo** keeps its data in `~/chloectx-demo` (an existing `~/engram-demo` is still used).
- **`docs/guide/run.py`** gives each page an empty models folder of its own (`--shared-models DIR` shares one), so a page that needs a model must fetch it; the agent-memory recipe now fetches its model.

#### Fixed

- `chloedb-crawl serve --local` (and `crawl --local`) stopped by SIGTERM just after start no longer leaves its chloedb-servers running: the signal handlers are installed before the cluster starts.

(#457)

### Understanding

#### Added

- **Understanding in the command center: Enrich rows and Enrich queries.** The Query understanding page is now *Understanding*, and it has two workflows.
  - *Enrich rows* adds fields that a model makes of each row's text, one step per field. The steps are a language, main phrases, near duplicates, the names it mentions from another table, a classifier's class, a language model's text from a prompt, a vector for meaning, and a caption or transcript of a file. Each step is tried on sample rows first. Then the steps are added in one `ALTER TABLE`, and the rows written before are filled in, 200 rows a write, with progress and a stop button. A table shows how many rows have each field and its commonest values.
  - *Enrich queries* reads a search in steps: spelling, the values its words name, its class, its language, the names it mentions, and what the caller knows (`country = CL`). It adds what it reads to the request as filters and `bm25`/`rrf` options. You see the request as SQL with its parameters, run beside the words alone, and can save it as a search whose filters are parameters (`NULL` leaves one out).
- **`POST /v1/derive/preview` takes dictionaries and classifiers.** `dict:<table>.<field>` returns the names a text mentions, read from the table now (`table` and `field` set the analyzer). `classifier:<name>` returns its label, or with `many` the labels a `KEYWORD[]` field would hold, plus `top`.

#### Known

- On a cluster, a `dict:` field is made on each shard from that shard's part of the names table, so a row misses the names other shards hold. The page warns about this on a coordinator; a fix belongs in the write path.

(#492)

#### Fixed

- **Query understanding through a coordinator.** `bm25(.., understand => ..)` and `POST /v1/understand` failed on a cluster with "LIMIT and OFFSET reach row 200000; a query returns at most 10000 rows": the coordinator read a keyword field's dictionary (up to 200,000 values) as a client's query, held to `--max-rows`. It is now the server's own read, as a single server's always was.
- **"Did you mean" on a cluster.** `POST /v1/correct` on a coordinator answered "a coordinator does not correct text yet". It now corrects as one server does, by its shards' term statistics summed: the words' own document counts, then, for a word in none, its neighbours' (each shard's nearest).
- **ChloeCtx in the command center.** A space's card counted every note ever written, so a space whose notes were all replaced or retired said "11 notes" and opened to "No notes yet": it counts the current notes now (`live_by_kind` in `GET /v1/spaces`), says how many are only in its history, and the empty list offers the history. The Receipts and Blame tabs, which read a machine's local stores, are left out where `chloectx app` has none (a cluster's).

#### Added

- **A ChloeCtx space as an MCP server.** `POST /v1/apps/chloectx/v1/spaces/<space>/mcp` speaks the Model Context Protocol (streamable HTTP, JSON answers) with three tools: `ask` (the context for a task, within a budget of tokens), `remember` (a note, which may replace an older one) and `notes`. A space's page has a "Connect Claude Code" card that makes a key that may write only the space's table (`ctx_<space>=write`) and shows the `claude mcp add --transport http ...` line to paste.
- **Keys limited to tables reach an app that holds them to their grants.** An app whose `app.json` says `"limited_keys": true` is called by such keys too (with any grant that reads); other apps still refuse them. `chloectx app` is one: a space is its table `ctx_<space>` (read to read and ask it, write to change it), the incidents `ctx_incidents`, the Inspector `chloectx_memory`; creating a space and building apps from one take an unlimited admin's key, no limited key reaches the machine's local stores, and `GET /v1/spaces` lists the spaces the key may read.
- **The Inspector's empty state is a setup**: a button makes the table `chloectx_memory` and a key that may write only it, and shows the `chloectx sync setup` command with the key filled in (ChloeCtx 0.2.0 or later).

(#483)

### Read routing

- A coordinator keeps each replica's recent round trips, calls in flight,
  errors and reported load, shown in `/v1/cluster` (`observed`) and the
  `chloedb_replica_*` metrics. `--route least-cost` sends a read to a
  replica drawn at random when it costs clearly less (round-robin stays the
  default), `--hedge-adaptive` hedges after the replica's own tail instead
  of after `--hedge-ms`, and `--prefer-caught-up` routes reads that must see
  a write by each replica's round trips of such reads. Measured by
  `chloedb-route-bench` (results in `chloedb-server/results/route-*/`).
- Performance and correctness baselines of ChloeDB's write-ahead log,
  flushes and merges, and of ChloeCtx's recall; the test Postgres stops by
  its fast shutdown, so macOS no longer runs out of shared memory segments.

(#493)

### ChloeCtx

- `chloectx ui` shows in its header the version and commit it runs and
  whether the checkout it was built from has a newer one, and Update runs
  `chloectx upgrade` from the page, its output shown as it goes (#494).

## 0.3.1

Fixes from the second audit (13 Nov) and what was built beside them:
`chloedb-server-v0.3.1`, `chloedb-py-v0.3.1` and `chloedb-js-v0.3.1`
(the Java package and ChloeCtx are built from source). The file format
stays 3 and no file feature is new. Three plan features are new, and a
plan carries one only when its query uses it: `asof_join` (`ASOF JOIN`),
`date_bin` (a `GROUP BY date_bin(..)` or `date_trunc(..)` on the columns)
and `approx_percentiles` (`approx_percentile`). A 0.3.0 shard refuses each
by name, so upgrade the shards before the coordinators. The shard protocol
stays 20: `JOIN_KIND_AS_OF` and `QueryRequest.skip_steps` are additions an
older server ignores or refuses by name. A replica no longer installs a
copy of its primary that arrives late, which is what lost 34 acknowledged
writes in the simulator's nightly seed (#477).

### What changes by default

- chloedb-server keeps answers between queries (`--answer-cache-mb`, 64
  MiB; `0` turns it off), allocates with mimalloc, and leaves a query's
  profile steps out of its answer unless asked for (`"steps": true`) (#456).
- A table with `retain` merges its segments only within windows of time
  (#463).
- ChloeCtx keeps an automatic snapshot at most once an hour, not after
  every recall (#473).
- The app engine gives each app a key of its own, and a key limited to
  tables can no longer read other tables' vectors through `vector_of` or
  `embed` (#476, #462).
- The command center is reorganised around what people do with their data
  (#471).

### Security

- The app engine gives each app a key of its own (audit N-G5, N-G6, part of
  N-G7, and N-G3). An app's `app.sql`, inserts, updates and removes go to ChloeDB with
  a key limited to the app's tables (`app_<id>_*`, write, and `sa_numbers`,
  read, for the library's table functions), so the server refuses another
  app's rows, the engine's people, sessions and settings, and every other
  table of the cluster, whatever the engine's SQL rewrite lets through; the
  engine no longer runs the statement through `/v1/explain` first on that
  path. The keys are made from the engine's own key, kept sealed in
  `sa_settings`, reused across restarts and made again when the server no
  longer takes them (`STRATA_APPS_KEYS=shared` keeps the old behaviour).
  An engine given the admin token makes itself a key limited to `sa_*` and
  `app_*` (`chloedb-apps`) and follows the feed of changes with it too (it
  keeps the token for the feed only on a server from before limited keys
  could read it), and says so in its log; an engine that cannot give its apps keys says so at start
  and in `GET /api/status` (`keys`). The chloedb-server the engine starts on
  its own (`run.sh`) now takes a token, kept in `data/strata.token`. (#476)
- `chloedb-server`: keys a key makes (`POST /v1/keys/own`, `GET`, and
  `DELETE /v1/keys/own/ID`). A key with the admin scope on some tables makes
  keys within them, named `<its name>/<name>`; they expire with it, open
  nothing once it is revoked (its revocation revokes them), and make none in
  turn. Their digest is marked `d:` in the keys file, so a server from
  before them never admits one. `/v1/whoami` says `auth.own_keys`. This
  touches auth (one-way door). (#476)
- `chloedb-server`: a key limited to tables may follow the feed
  (`GET /v1/feed`) of the tables it may read: each transaction's operations
  on them, an empty `ops` when there are none, and no `sources` or
  `unknown`; it may not ask for a copy (`copy=true`). `/v1/whoami` says
  `auth.limited_feed` (audit N-G6). (#476)
- The app engine: apps' own model calls (`app.ask`) have a share of the
  spending limit of their own (`STRATA_APPS_ASK_CAP_USD`, a quarter of
  `STRATA_APPS_SPEND_CAP_USD` by default) and builds the rest, so that a
  published app's visitors cannot stop the studio's builds, nor builds the
  apps; one app's calls stop at `STRATA_APPS_ASK_USD_PER_DAY` dollars a day
  (default 2), whatever their count; and a published app suggests changes
  from its use rather than building them, unless its owner sets its mode to
  build (audit N-G3). `GET /api/status` shows `spend.asksUsd` and
  `spend.asksCap`. (#476)
- `chloedb-server`: a key limited to tables may read a stored table
  function: its body is written out and checked by the tables it reads, as
  the engine runs it (before, every table function was refused). Such a key
  with the admin scope on some tables may `CREATE FUNCTION` one no one has,
  or the same definition again, when its body reads only tables it may read
  and calls no model; it replaces and drops none. (#476)
- A key limited to tables can no longer read another table's vectors through a request parameter. `{"vector_of": key, "table": "t"}` now needs read on `t`, and a limited key has to name the table. `{"embed": ...}` (and `POST /v1/embed`) needs read on the table whose model it uses. This applies to `/v1/query`, `/v1/sql` and `/v1/explain`. (Audit N-D1.) (#462)
- Applying a relevance-autopilot proposal now runs only a `CREATE OR REPLACE RANK PROFILE` of the proposal's own profile on its own table, rebuilt from what the stored statement parses to. Any other statement is refused (`DROP TABLE`, `CREATE FUNCTION`, another table or profile, a second statement). A table grant given by a pattern (`*`, `rel*`) can still read `relevance_proposals` but can't write it. Only a grant that names the table writes it. (Audit N-D2.) (#462)
- A key limited to tables can no longer send an operation with a `"source"` (a stream's offset) or an `"op": "source"` mark to `/v1/write`. Before this, such a key could move the offset of any stream, so that the stream's later events were dropped as replays, even in tables it can't write. (Audit N-D3.) (#462)
- The apps that chloedb-server starts itself (crawler, Sheets, ChloeCtx) no longer bind a port that the server picked and then released. The server binds each app's loopback socket, holds it while it runs, and hands it to the app as file descriptor 3 (`CHLOEDB_LISTEN_FD=3`) at every start and restart. Before this, another local process could take the port in the gap, receive the app's token and the forwarded requests, and serve the command center's pages for that app. `chloedb-crawl serve`, `chloedb-sheets serve` and `chloectx app` accept the inherited socket. Otherwise they bind `--listen` / `--port` as before. These programs need to be from the same release as the server. (Audit N-P1.) (#462)

### Added

- `chloedb-netops`: network operations on ChloeDB, an app of the command
  center (`chloedb-server --app netops=URL`, the operator's `spec.netops`,
  port 7840, a key limited to `netops_*`). A map of the whole network,
  from regions down to one customer's router (about 200,000 simulated
  objects of 18 kinds), that reads only its window at its level of
  detail, colours each place by what is broken inside it, walks the graph
  around an object that is clicked, and takes a search in a sentence
  ("changed in the last week about a firmware upgrade by m.rossi with
  latency above 15 ms, behind ag1-fra") that it compiles into one
  statement, rewritten by the server's `llm()` when it has a model. The shift's home page (a
  handover written from the tables and one list of what waits), a lookup
  of anything a ticket names, a Diagnose page (the stack by place with
  the lowest broken layer named, one broken thing asked of the graph, the
  logs, the history and past incidents, and a trace hop by hop), incidents grouped over the topology graph
  with a root-cause agent that keeps its SQL, a board per incident that
  starts from its cause's template and that people extend by asking for a
  panel, alerts with owners and silences, changes carried out and checked
  against the network before them, a customer's "is it the network?",
  carriers' maintenance notices checked for collisions, the topology read
  from the logs, and a leadership view of the same tables. It ships with a
  simulated carrier network and six faults; `--no-simulate` reads the
  network from the inventory tables and judges whatever feeds write
  ([docs](docs/chloedb/netops.md)). (#458)
- `chloedb-server`: connectors, other systems' data kept coming into
  tables. Pulls on a schedule: Prometheus rollups (PromQL over each new
  minute, series joined by labels into one row per subject and step), Jira
  issues by JQL, ServiceNow records through the Table API, Oxidized device
  configs (each changed node a version), any REST API that answers JSON;
  pushes: raw emails (with carriers' BCOP maintenance fields read out),
  Alertmanager's webhook, any JSON webhook, Oxidized's hook. A map turns
  records into rows (paths, templates, conversions), fitted to the target's
  columns and upserted by key; presets write the network operations app's
  tables. Configs are rows of `chloedb_connectors`, runs of
  `chloedb_connector_runs`; secrets stay in `<db>.connectors.json`
  (`--connector-secrets`) or `env:` names. A pull writes with a key the
  server makes for it, limited to its target; a push connector gets a
  limited key `connector:<name>` that the sending system holds. Pulls run on
  a server of its own or the coordinator that pages (`--connectors off`).
  `/v1/admin/connectors`, `POST /v1/connectors/NAME/push`, a dry run
  (`/v1/admin/connectors/test`), and the command center's Data > Connectors
  page with each connector's health. [docs](docs/chloedb/connectors.md) (#464)
- `ASOF JOIN ... MATCH_CONDITION (s.ts > u.ts) ON ...`: each row joined to the latest row of the other table from before its time (point-in-time joins for training sets), on the row operators and on columns in the DataFusion path. The shard protocol gains `JOIN_KIND_AS_OF`; an older shard refuses such a plan. (#466)
- `chloedb/results/feature-store`: a benchmark against Feast 0.68 (training sets and online serving). (#466)
- `examples/feature-store`: a feature store demo, the command center app `features` (streaming features on write, a batch SQL job, snapshots, ASOF JOIN training sets, models as rows, serving by key), with a feature builder: workflows defined as rows, chained, drawn as a map and searched by their statistics. (#466)
- `chloectx upgrade` installs the newest version from the checkout ChloeCtx was built from: it pulls the checkout (fast forward only; `--no-pull` builds it as it is), stops the old daemons, rebuilds with `install.sh --no-wire` into the binary's own prefix, writes the user-level install again (new hook events; the distillation model is kept) and runs `doctor`. `--check` only says whether a newer version is there. The binary records the commit it was built from (`chloectx --version` names it). (#480)
- `chloectx doctor` says how many commits on the checkout's upstream branch change ChloeCtx or ChloeDB since the build, warns about the store folder's old name, and counts the stores (and the ones `prune` would remove). (#480)
- `chloectx stores` lists every store on the machine with the project it records, its size on disk and when it was last used. `chloectx prune` removes the stores nothing will use again: their project folder is gone, or is a temporary folder unused for a day, or they name no project and were unused for a month (`--older-than DAYS` adds idle ones). It lists them and asks first (`--yes`, `--dry-run`); a running store is never removed. Stores from before `project.txt` get one written from their `daemon.log`. (#480)
- SQL `date_bin(width, t [, origin])`: the start of the bucket of a fixed width (`'5 minutes'`, `'30s'`, `'1 hour'`, `'1 week'`) that a time is in, counted from an origin (1970-01-01 without one), as PostgreSQL's. A `GROUP BY` of it over a `TIMESTAMP` field runs on the columns, each row finding its bucket by number; so do `date_trunc` to a second, minute, hour, day or week. OpenSearch's `date_histogram` with a `fixed_interval` runs as one instead of a subquery. Plan feature `date_bin`; proto `FUNC_DATE_BIN`. On 20M synthetic log lines (4-core Xeon 2.1 GHz): 5-minute buckets by severity over 24 hours in 35 ms rather than 1,343 ms the old way; hourly buckets over 7 days in 52 ms rather than 255 ms. (#467)
- SQL `approx_percentile(f, p)`: `PERCENTILE_DISC(p)` within 1%, from a DDSketch of at most 4 KB a group however many rows; sketches merge exactly across segments and shards, and travel as versioned bytes. Several fractions of one field share one sketch. Plan feature `approx_percentiles`; proto `AGG_FN_APPROX_PERCENTILE`. On the same data: p50/p95/p99 over 30 days in 406 ms rather than 2,058 ms exact, and per 5-minute bucket by service in 90 ms rather than 114 ms, all within 1%. (#467)
- The shard simulator has a knob for calls held up while faults run (`held_per_mille`, 3 by default, as before), a setup where copies often arrive late (`chloedb-sim --late-copies`, which CI now runs on 1,000 seeds per change and 10,000 per night), a `late_copies` count, and the bug flag `installs-a-late-copy`, which that setup finds again. (#477)
- `chloedb-server --otlp-retain PERIOD`: the retention of the tables
  OpenTelemetry logs make (`otel_logs`); without it they are kept for
  ever, as before. (#463)
- `examples/log_series_bench.rs` and results/log-series: log tables over
  time (merges and retention while streaming, time-window queries). (#463)
- `PACK :n TOKENS`: the budget may be a parameter. (#475)
- `chloedb-pack-bench`: PACK against the top rows of a ranking cut in the client, on BEIR, with the estimate of tokens compared with cl100k_base and o200k_base (`chloedb/results/pack/`). On SciFact at 500 tokens PACK finds 0.741 of the relevant documents where the top rows cut to fit find 0.647, and at 2,000 tokens and more the two are within a hundredth (Intel Xeon 2.30GHz, 4 vCPUs). (#475)
- FOLLOW's second and third hops follow the rows taken at the hop before, not every row read (as the reference says). (#475)
- ChloeDB: rollups. `CREATE ROLLUP logs_5m ON logs AS SELECT
  date_bin('5 minutes', ts), service, COUNT(*), SUM(latency), MAX(latency)`
  makes an ordinary table of counts, sums, least and greatest values per
  bucket of time and group, kept up to date as rows arrive: a refresh
  compares each recent bucket's count of rows with the table's (read from
  the time column's index) and folds again, in one transaction, the
  buckets that differ, late rows within `OPTIONS(lateness = ...)` (10
  minutes by default) included; `REFRESH ROLLUP name [FULL]` and
  `DROP ROLLUP` too. chloedb-server refreshes every rollup every 5
  seconds and checks every bucket every 5 minutes; an embedding program
  calls `Db::spawn_rollups`. Buckets older than the table's `retain` keep
  what they counted, and `OPTIONS(retain = ...)` gives the rollup its own.
  The definition is a stored function (`rollup__<name>`), so nothing new
  is written on disk. Queries name the rollup; a coordinator over shards
  does not run rollup statements yet. (#484)
- `log_series_bench rollup DB`: a rollup's first fold, its refreshes, and
  time-bucket queries of it beside the same queries of the table. (#484)

### Changed

- A speed gate (`.github/workflows/perf.yml`, `scripts/perf-gate.py`): two commits built and timed in turns on one runner, BM25 through SQL on the relevance gate's collections, loads, and `chloedb-server-bench` on SciFact over HTTP; a measure fails when the head is more than 10% slower on its median and slower in every round. Nightly (main against main 24 hours earlier; a red night opens an issue), on a pull request labelled `perf`, or by hand. (#460)
- The framework adapters (`chloedb.langchain`, `chloedb.langgraph`, `chloedb.llama_index`) and `chloedb.dbapi` read a server through a `Session`, one per adapter, so their reads see their own writes through a coordinator and from a replica (start the servers with `--seq-no`); a `chloedb.Client` given to one is used through its session (audit N-H1). (#474)
- LangGraph's checkpoint and writes tables are made with `shard_by = 'thread_id'` and `cluster_by = 'thread_id'`, so a thread's rows sit on one shard of a cluster and a task's writes go in one transaction; `get_tuple` and `put_writes` read the thread's shard only. Tables made by 0.3.0 or earlier keep their placement by key (audit N-H2). (#474)
- `chloedb.dbapi`: `with conn:` commits and leaves the connection open, as `sqlite3` does (it used to close it); `contextlib.closing(conn)` closes it (audit N-H4). (#474)
- `chloedb.dbapi`: a `TIMESTAMP` column comes back as a `datetime` in UTC (it was ISO text); `Binary()` and `bytes` parameters raise `NotSupportedError`, since ChloeDB has no binary type (audit N-H6). (#474)
- `chloedb.dbapi`: a `SELECT` through a server is read a page of 10,000 rows at a time as it is fetched, past `--max-result-window`, as a file reads it; `rowcount` is -1 until the count is known. A ranked query without `LIMIT`, cut to its best 100 rows, warns and puts a `Warning` in `cursor.messages` (audit N-H5). (#474)
- LangChain relevance scores (`similarity_search_with_relevance_scores`, `similarity_score_threshold` retrievers): by vector the cosine itself (it was `(1 + cos) / 2`), hybrid the fused score over its best (1 for a document first in both rankings), by words `NotImplementedError`; chosen by the search's own `mode=` (audit N-H7). (#474)
- The Python client keeps one connection per thread alive between requests instead of a connection per request (over https, a TLS handshake each), and sends NaN and infinity as `null`, as a database in the process stores them (audit N-H6, N-H11). Through `$HTTPS_PROXY`/`$HTTP_PROXY` it uses `urllib` as before. (#474)
- Operator: with `spec.upgrade.mode: on`, the app engine (`spec.apps`, at a
  tag) runs its tag's newest image by digest (`status.upgrade.apps`), so a
  new `chloedb-apps:latest` restarts it within `checkMinutes` without a
  `kubectl rollout restart`. An installed operator does not upgrade itself:
  apply `operator.yaml` again to get this. (#476)
- When distillation has been failing (most often an expired `claude` CLI login), a session's start says so: a message to the person (the hook's `systemMessage`) with the fix (`claude auth login`, or `chloectx doctor`), and a line for the model at the top of the briefing. Until now an install in that state looked like a working one while nothing was written down. A newer version in the checkout is mentioned once a day. (#480)
- `chloectx --help` lists the eleven everyday commands (install, uninstall, doctor, upgrade, ui, ask, search, remember, stores, prune, stop); the others still work and are named below the list by what they are for. (#480)
- `chloectx upgrade` moves `~/.local/share/engram`, the store folder's name before the rename, to `~/.local/share/chloectx` (with the daemons stopped, and not when `CHLOECTX_HOME` is set), leaving `engram` as a link to it. (#480)
- ChloeCtx keeps an automatic snapshot at most once an hour by default (`receipts.snapshots.min_interval_secs`, was 0: one after every recall that showed records). Each snapshot flushes the store and reads every segment, and one per turn more than doubled the store's write amplification and its disk use. Set it to 0 in the store's `config.json` for a snapshot after every recall (audit N-F7). (#473)
- ChloeCtx writes a recall's receipt after the hook has answered, not before (audit N-F14). The receipts file flushes every 256 receipts and when the daemon stops, merges in the background, and keeps receipts 90 days and at most 20,000 (`receipts.retention`), pruned when the daemon starts (audit N-F3). (#473)
- `chloectx receipts off [--purge]` and `chloectx receipts on` turn receipts and automatic snapshots off or on for a project's store. Without `receipts.enabled` in `config.json`, `CHLOECTX_RECEIPTS=0` now holds whichever process started the daemon: each hook sends it (audit N-F6). `chloectx stats` shows the receipts' and snapshots' bytes (audit N-F9). (#473)
- A capsule's manifest names the project's folder, not its absolute path (audit C7); `pack --max-records` keeps the newest records, not the oldest (C3). (#473)
- ChloeDB: a table with `retain` merges its segments only within windows
  of time, a thirtieth of the retention each (from a second to a day), by
  each segment's newest row, so a segment leaves the table whole once its
  newest row is past the retention instead of being rewritten as its rows
  expire. Segments with no live rows left are dropped together, unread
  (and a cold one unfetched). On a stream kept 190 seconds for 10 minutes
  (results/log-series), rows rewritten per row fell from 2.99 to 1.91 and
  expired rows still on disk from 964K to 409K. Tables without `retain`
  merge as before. (#463)
- `PACK n TOKENS` is marked experimental in the reference: what it reads and how it chooses may change. (#475)
- `PACK` reads its candidates deeper each time from the query's `OFFSET` (the first 64, 320, 1,344 rows ...), passing over the rows seen by their keys, instead of reading the next page. A fused ranking (`rrf`, a weighted sum) ranks to a depth that grows with the `LIMIT`, so its pages at `OFFSET` were pages of different rankings: rows could be missed, and a short second page ended the pack with most of the budget unspent (audit P2, N-B7). A table without a primary key is still read page by page. (#475)
- `PACK`'s excerpts and word `DEDUPE` analyze text with the column's own analyzer (its field's, else the searched field's, else `english`): terms match by stem, not by a prefix of four letters (`policy` no longer matches `police`), and Chinese, Japanese and Thai are compared by their words (audit P9, N-C3). A term in more than a quarter of a text says nothing of where to cut it, and is passed over. (#475)
- `WITH RECEIPT` answers the rows skipped with their text columns `NULL`, so that the text of the rows answered still fits the budget (audit P6). (#475)
- A row with no text to count is `skip: no text`, not `skip: over budget` (audit P5). (#475)
- A text column that is `NULL` in the first rows read and text later is counted from the row it first holds text (audit P8). (#475)
- `count_tokens` counts a token per three letters in alphabets other than Latin (Cyrillic, Greek, Arabic, Hebrew, Devanagari and the rest), where it counted one per six and undercounted them about three times. Measured against o200k_base it is now 9% to 27% over in every language sampled; against cl100k_base it is still under in Arabic, Hindi, Chinese, Korean and Russian (`chloedb/results/pack/`). (#475)
- chloedb-server allocates with mimalloc (feature `mimalloc`, on by default): a coordinator uses 25-35% less CPU per query, a shard 5-15% less. (#456)
- query answers (`/v1/query`, `/v1/sql`, `/v1/plan`) carry their profile steps (`stats.steps`) only with `"steps": true` or `"profile": true`; `/v1/explain` and the list of queries keep them. A coordinator asks its shards to leave theirs out (`QueryRequest.skip_steps`). (#456)
- chloedb-server keeps answers between queries (`--answer-cache-mb`, 64 MiB by default, 0 turns it off): the same query on the same version of a table is answered from memory, and any write makes it compute again. Queries that read the clock, random numbers, `uuid()` or a model are never kept. Metrics `chloedb_answer_cache_*`. (#456)
- An idle HTTP connection costs about 31 KB instead of about 300 KB (the router was copied for each connection), and the server raises its soft open-file limit to the hard limit when it starts. (#456)

### Fixed

- Vector and hybrid queries no longer pay for static embeddings (#433): a query with `similarity(emb, :v)` whose parameters hold only vectors and numbers is no longer parsed again (the server's parse was uncached) and copied twice to find no text to embed. Server p50 on SciFact, one database: vector 0.632 ms -> 0.536 ms (0.2.0: 0.518), vector + filter 0.485 -> 0.386 ms (0.377); hybrid queries win back about half (Xeon 2.1 GHz, 4 CPUs, release builds, 3 rounds in turns). (#460)
- Closing one `chloedb.dbapi` connection no longer closes the database under the adapters and other connections of the same path: each opening counts, and the last to let go closes it (audit N-H3). (#474)
- LlamaIndex `delete_nodes(node_ids=[], filters=...)` and `delete_nodes(filters=MetadataFilters(filters=[]))` delete nothing (they deleted every node the filter kept, or every node); `get_nodes(node_ids=[])` returns none (audit N-H8). (#474)
- LangGraph `list_namespaces` works past `--max-groups` (65,536 namespaces): it reads the namespaces in order a page at a time when grouping them fails (audit N-H9). (#474)
- LangGraph filters find checkpoints and store items whose metadata or value is past what a JSON field takes (1,000 values, 16 levels, 256-byte paths): what fits is indexed, top-level values first, where nothing was before (audit N-H10). (#474)
- `ChloeDBStore` no longer switches the database-wide `set_embed_later` on and off around writes of items without text, which let another thread's rows go without vectors and turned off a setting the caller had turned on (audit N-H13). (#474)
- A `chloedb.dbapi` connection reads the tables' column types once, and again after a `CREATE`, `ALTER` or `DROP` it runs, not with every `SELECT` (audit N-H11). (#474)
- The PyPI placeholder (`packaging/chloedb/python`, 0.0.2, not yet published) names ChloeDB and the release install command; the published 0.0.1 still names Strata. The quickstart installs the wheel from the release, since `pip install chloedb` alone takes the placeholder until PyPI publishing is switched on. (#474)
- Prompts are stored without the envelopes some harnesses wrap them in (`<system-reminder>`, `<wake>`, `<coordinator-relay>`, an HTML-escaped `<session-context>`, slash-command markup): the person's words inside are kept, the machinery dropped. Stored with them, they were recalled into later prompts as if they were the conversation, and a reminder's "never ..." was picked as the user's rule. Recall, the briefing and the distiller's related notes pass over records stored that way before, and a daemon retires the rules taken from them once, when it first starts on a store. (#480)
- `memory_forget` rewrites the store's segments that held the record without it before it drops the automatic snapshots, and no snapshot is taken meanwhile, so the next snapshot no longer copies the forgotten text for another 30 days (audit N-F2). Forgetting a prompt also removes it from the query of its receipts, and scrubbed receipts drop the forgotten record's text hash (R2). With receipts off, `memory_forget` and `chloectx redact` still scrub the receipts kept before (N-F6). Receipts are scrubbed a page at a time, in one commit per page (R6). (#473)
- Snapshot retention runs when the daemon starts and before each new snapshot, whatever the settings say now, so turning receipts off or leaving a project no longer keeps its snapshots for good (audit N-F4). What lies under `snapshots/` and belongs to no snapshot (a checkpoint a stopped daemon left half written) is removed when the daemon starts; a snapshot whose predecessor does not read is kept as a full copy instead of failing every time; removal writes the index before deleting directories (N-F12). `chloectx stop` waits for the work after each answer (the prompt's own record, a snapshot being written). (#473)
- A snapshot records the commit readers saw when the checkpoint took the writer lock (`CheckpointStats::source_txn`, new in ChloeDB), so a receipt never replays "exactly" on a snapshot holding a later write (R3). A receipt's recency reference is the one ranking used (N-F10). Receipts name the ChloeCtx version that wrote them, and a replay says when its embedder or version differs (R4). (#473)
- A mounted capsule's answers are no longer recorded into the receiving project's memory, whatever its MCP server is named (audit N-F5). `chloectx mount` refuses a capsule that carries stored functions or rank profiles, which could call a model with the user's key from inside `capsule_sql` (N-F8), and one whose embedder differs from its manifest's unless `--embedder` is given (C4). The manifest's title and topics are cut to one bounded line and presented as the packer's words (C2). The mount's copy and the pack's backup are made in `mkdtemp` directories, and the copy is the file whose hash is checked (C5, C6). A record redaction changed while packing gets a vector of its redacted text (C1). (#473)
- `chloectx blame` attributes a line to the newest edit that added it as a whole line (not one that only kept it around its change), reads the receipts of the edit's turn and the three turns before it instead of the session's oldest 500, and resolves a relative path from where the command runs, or from the project in a daemon request, never from the daemon's own working directory (R5, N-F11, N-F13). (#473)
- A replica no longer installs a copy of its primary that arrives late. A copy whose call gave up (its timeout, or a broken connection) could still reach the replica after a newer copy and the transactions that follow it, and replace what the replica had applied and acknowledged since; a later election that replica won lost those writes. This is what the cluster simulator's nightly seed 624036738 found (34 acknowledged writes lost; audit E-0g, N-E1). `Database::install_copy` now refuses a copy that ends before the database's last transaction when that transaction is of the copy's term, and changes nothing; the new `Database::install_copy_over` (and `Db::install_copy_over`) installs it anyway, which a replica does only when it holds another transaction than its primary's (`differs`). The term check and this one now run before anything is changed. (#477)
- `chloedb-server`: a copy's install holds the replica's call lock until it ends, also when the primary's call ended early, so no other call of the primary is taken while a copy is going in. (#477)
- An excerpt never spends more than its room: text without spaces (Chinese, Japanese, Thai, a URL, base64, a minified line) is cut inside the run, in pieces of at most 8 tokens, where it used to be returned whole, so one row could overrun the budget by any amount (audit P1). (#475)
- On a server, a `PACK` statement is admitted once by the Contain rules, not once per read (a rule of one query at a time refused its second read with 429), and its reads share the statement's deadline; the reads of steps of `chunks_of`, understood and expanded words are admitted once too (audit P3). (#475)
- On a server, every read of a pack is of one snapshot: a lease held for the pack and released after it, or the client's `snapshot` (kept for the client with `hold`). A coordinator's shards still each read their latest version (audit P4). (#475)
- An embedded `Db` runs a pack's reads as the parsed queries they are, not printed and parsed again (no parse-cache churn, and `- -1` works); a server prints them and refuses one that does not read back the same, as `llm()` does (audit N-B8, P7). FOLLOW's reads take the keys as a parameter, so their text is the same for every row followed. (#475)
- `PACK ... LIMIT 0` answers no rows with the pack's columns on a server too (it was HTTP 400), and `Db::query_leaf` (Python's `query_arrow`) runs `PACK`, `chunks_of`, understood and expanded words and `llm()` as `Db::query` does, where it failed with a syntax error (audit N-B9). (#475)
- A key limited to tables can run `PACK`: it is judged by the query without its clause (audit N-D6). (#475)
- `similarity(field, embed(*))` and `similarity(field, embed(name => ..))` no longer panic the caller's thread (or kill the server request); they are refused with "embed(..) takes text" (audit N-B1). (#455)
- `embed('hash:N', ..)` refuses N = 0 (a division by zero) and N above 65,536, which used to allocate N floats; `hash:N` models in schemas get the same check. New `VECTOR(N)` fields take 1 to 65,536 dimensions (`chloedb::schema::MAX_VECTOR_DIM`). The embedded `Db` now keeps at most 16 models opened by `embed('model', ..)`, as the server does. (#455)
- `LIKE` and `ILIKE` check the query's deadline and cancel flag every 1,024 values inside a segment, in substring-index parts, in segments without the index and in the few-rows path, so `timeout_ms` and `Db.interrupt()` stop a scan of a large segment (audit N-B2). A `LIKE '%x%'` on a KEYWORD field without a substring index, which the operators filter row by row, now also stops: the projection of a segment's rows checks the deadline too. (#455)
- `chloectx blame path:line` reads only a regular file inside the project, after links and `..` are resolved, of at most 8 MiB; `../`, absolute paths outside the project, links out of it and `/dev/zero` are refused (audit F R1). (#455)

### Experimental

- A tenant's write that the store refused although it had taken it is
  now reported committed, not `NotWriter`. A store's client may send a
  conditional put twice after an answer it lost and report the store's
  refusal of the second; the handle now loads the tenant again and looks
  before it says that nothing was written, so the caller does not make
  the write a second time (audit finding N-P12). (#469)
- The directory store's `delete` takes the lock its conditional writes
  take, so a write of `HEAD` that races a tenant's deletion cannot put
  `HEAD` back after it. (#469)
- `POST /v1/admin/tenants/:id/sql` decides whether a statement reads by
  how the engine binds it, not by its first word, and runs reads through
  what cannot write. A query that begins with a comment no longer makes
  the server the tenant's writer (audit finding N-P15). (#469)
- The blackboard's tools over HTTP and MCP refuse `claim`, `complete`,
  `fail` and `post` without a `key` (and `add_tasks` of a task without
  an id): 400, or an invalid-parameters error over MCP. A key is now its
  agent's own, so two agents that choose the same key are not answered
  each other's calls. Keys written by 0.3.0 are not found under the new
  names: a call sent again across the upgrade is applied again. (#469)
- The design for 0.6.0 now begins the accepted `STRARC01` format at
  version 2. Version 1 stays what experimental tenants write, as 0.3.0
  does, so nothing written now is taken for the accepted format later,
  and tenants written by 0.3.0 are read as before. (#469)

### Command center and CI

- The command center is built around what people do with their data: a
  page per table (search by words or meaning, data, quality, what uses it,
  storage), saved searches kept as stored functions and callable from
  code or MCP, a Start here page, Ask (Ctrl+J, the server's agent), and
  navigation by job (Build, Watch, Run the cluster, Apps, Labs). Every
  table function is an MCP tool (`fn_<name>`), and an alert rule can watch
  a SELECT. (#471)
- Query understanding bounds its own reads, quotes expansion's names and
  bounds mining. (#459)
- CI runs only the jobs a change needs, builds chloedb-server once per
  source tree, builds the guide's programs in parallel and keeps them by
  their source trees (the docs job from 22 minutes to under 2), splits the
  DataFusion and image jobs, runs no macOS job on pull requests, and can
  overflow to an on-demand DigitalOcean runner. (#461, #465, #468, #470,
  #479, #481)

## 0.3.0

Apps on by default, each with a key limited to its own tables, and new
ways to search: `chloedb-server-v0.3.0`, `chloedb-py-v0.3.0` and
`chloedb-js-v0.3.0` (the Java package and ChloeCtx are built from source).
The file format stays 3. Five features are new, and a database carries one
only once it uses it: `analyzer:<name>:1` (a field of one of the new
analyzers), `substring:1` (a substring index), `sparse_embed` (a SPARSE
field a model fills), `derive_keyword` (a KEYWORD field derived from a
text) and, in a plan sent to shards, `terms_syntax` (query expansion). A
0.2.x build refuses each by name and opens or runs everything else, so a
database that uses none of them goes back to 0.2.1 unchanged. The shard
protocol stays 20: upgrade the shards before the coordinators, since a
0.2.x shard refuses an expanded search by name. A key limited to tables is
marked in the keys file so that a 0.2.x server never admits it. Tenants
and the blackboard are experimental and off unless asked for.

### What changes by default

- The container image (`ghcr.io/chloetech-ai/chloedb-server`), run on its
  own or as a coordinator outside Kubernetes, now starts three more
  processes, as root as the image's own user is: the crawler, Sheets and
  ChloeCtx (`--start-apps auto`; `--start-apps off` or
  `CHLOEDB_START_APPS=off` keeps the old behaviour). On its first start ChloeCtx downloads its 129 MB embedding
  model from huggingface.co into the container's home, again after every
  `docker rm` unless that folder is a volume. The release archive, the
  installer and Homebrew ship `chloedb-server` alone, so they start
  nothing, unless `chloedb-crawl`, `chloedb-sheets` or `chloectx` sits in
  the same folder (as in `~/.local/bin` of a ChloeCtx user). Shards,
  replicas, TLS servers and pods in Kubernetes start none.
- A crawl the server starts names its tables with the prefix `crawl_`.
- Each started app holds a key limited to its own tables, never the
  server's admin token.
- Known gap, not a change: after the operator is upgraded to 0.2.1 or
  later, the pods of a cluster that already existed keep running as root
  until the cluster's settings change, because the operator runs as user
  10001 only the pods it makes from a new template.
- New format features, each written only by a database that uses one:
  `analyzer:<name>:1`, `substring:1`, `sparse_embed` and `derive_keyword`.
  A database that has one does not open in 0.2.x.

### Known issues

Found by the audit of 13 November 2026 and not fixed in this release; the
fixes are planned for 0.3.1. Until then:

- A key limited to tables can read the vectors of any table through a
  `vector_of` parameter, and can move the source offset of any stream so
  that its later events are dropped. Applying a relevance autopilot
  proposal runs the statement stored in `relevance_proposals`, a table any
  writer can write. Give limited keys only to programs you trust with
  every table's vectors, and apply only proposals you made.
- An app the server starts (`--start-apps`) is given a loopback port that
  the server chose and released: on a machine with other local users, a
  process that binds that port first receives the app's token and the
  requests sent to it. Run `--start-apps off` on a shared machine.
- A key with read scope can stop a server: `similarity(field, embed(*))`
  panics, `embed('hash:0', ..)` divides by zero and a very large `hash:N`
  asks for that much memory, and a `LIKE` scan is not stopped by the
  query's deadline inside a segment.
- `chloectx blame` (and its tab in the command center, with
  `--local-stores`) reads any file the daemon can read, of any size.
- `chunks_of` reads up to a million source rows whatever the query's
  `LIMIT`, outside `--max-rows`, and `POST /v1/synonyms/mine` takes its
  bounds from the caller with read scope.
- Tenants: a conditional put that landed but was answered as refused is
  taken as not committed, and a blackboard call without a `key` can then
  be applied twice.
- The query understanding, learned sparse fields, the relevance autopilot,
  `PACK`, tenants and the blackboard are experimental: their options and
  answers may change in a minor release.

### Apps

- chloedb-server: a server of its own or a coordinator starts the crawler,
  Sheets and ChloeCtx itself, as child processes, when `chloedb-crawl`,
  `chloedb-sheets` and `chloectx` are installed beside it
  (`--start-apps`, default `auto`; `off` starts none). Each listens on a
  loopback port, takes requests only with a token made for it at each
  start, and calls the server back with an ephemeral key scoped to its own
  tables, never with the server's token; it is started again when it
  exits, and stopped when the server stops. `GET /v1/apps` lists each
  one's process (pid, restarts, last exit, its last lines).
- chloedb-crawl: `serve --table-prefix crawl_` names every table a crawl
  fills with that prefix.
- ChloeCtx: the app has an **Inspector** page in the command center: the
  memory that `chloectx sync` copied into the cluster (`chloectx_memory`),
  by project, each session read as the conversation it recorded, filtered
  by kind and searched (BM25), as `chloectx ui` shows a local store
  (`GET /v1/memory`, `/v1/memory/sessions`, `/v1/memory/records`).

### Security

- `chloedb-server`: keys limited to tables, and no app holds the cluster's
  admin token (audit A1, A2, D-B10). `POST /v1/admin/keys` takes
  `"tables": {"crawl_*": "admin", "*": "read"}`: such a key reads, writes
  and creates only the tables its grants name, at their scope, never has
  the server's own admin, and is refused on every route that does not
  check tables (gRPC, OpenSearch and `/v1/mcp` too). Statements are
  checked by every table they name, with ChloeDB's own parser, and what
  names cannot show (table functions, `llm()`, `encode()`) is refused.
  Its digest is marked in the keys file, so a server from before limits
  never admits it. A server can make such a key for an app it starts,
  in memory only (`Keys::app_key`).
- The app proxy sends each app its own token (`--app-token-dir`, or
  `CHLOEDB_APP_TOKEN_<NAME>`; `CHLOEDB_APP_TOKEN` stays the fallback) and
  tells it who called: `x-chloedb-principal`, `x-chloedb-key`,
  `x-chloedb-scope`, `x-chloedb-tables`, set by the server alone.
- The operator gives each app beside a cluster (`spec.crawler`,
  `spec.sheets`, `spec.chloectx`, `spec.apps`) a key limited to its tables
  (`spec.<app>.tables`, with defaults per app) and a token of its own, in
  a Secret `<cluster>-<app>-credentials`, once the coordinators make such
  keys. The crawler that grows the cluster (`grow`, the default) keeps the
  admin token as its key. The operator's role may now create, change and
  delete Secrets, and the coordinators restart once to mount the apps'
  tokens. An operator whose ClusterRole is older (it upgrades neither
  itself nor its role) makes no such keys: its apps keep the admin token,
  and it logs that `operator.yaml` should be applied again.

### Added

- ChloeDB and `chloedb-server`: the relevance autopilot. It replays the
  queries the rank log kept that rank by a rank profile, on one snapshot,
  with the profile as it is and with the numbers a coordinate-ascent search
  finds (parameter defaults and the literals of its `ORDER BY`), and
  reports nDCG@k before and after on queries held out of the search, the
  queries made better and worse with a sign test, a bootstrap interval and
  latency, with a verdict. Grades come from the rank log's labels and a
  judgments table (`relevance_judgments`); the server can have Claude grade
  the rows no one graded first. A proposal is kept in `relevance_proposals`
  and applied as `CREATE OR REPLACE RANK PROFILE` only while the profile is
  the one it was measured against. `POST /v1/relevance/autopilot`,
  `/v1/relevance/proposals`, `Db::autopilot`, and the command center's
  Labs > Relevance autopilot.
- The rank log keeps each request's query and its bound parameters, in a
  new column `query` of the request's first row. A log table made before
  it keeps working and keeps no query (`ALTER TABLE rank_log ADD COLUMN
  query KEYWORD OPTIONS(indexed = false)` adds it).
- ChloeCtx: receipts and `chloectx blame`. Every recall (a prompt's context, the records a compaction gets back, a `memory_search` answer) writes a receipt: the store commit it read, its inputs, and each candidate's score parts, tokens and decision (`take`, `excerpt`, `knowledge`, `listed`, `skip: <reason>`, the words of ChloeDB's `PACK` receipts), with the text the agent was shown. After a recall that showed records the daemon keeps a snapshot of the store (a hard-linked checkpoint, pruned after 30 days), so `chloectx receipt ID` replays the recall exactly on the data it read, and on today's store. `chloectx blame path:line` finds the stored edit behind a line, its prompt and tool calls, and the records recalled before it, each checked against the store now (superseded, deleted, from the web). `chloectx snapshot keep|list|drop` keeps snapshots by name. Receipts are in their own file, `receipts.strata`; the store's file and format are unchanged. `memory_forget` and `chloectx redact` scrub receipts and drop the automatic snapshots. [Docs](docs/chloectx/receipts.md).
- ChloeCtx in the command center: Receipts and Blame tabs on its page (receipts with their candidates and replay, kept snapshots, blame of a line), over this machine's Claude Code stores when `chloectx app` runs with `--local-stores` (off by default: the stores are transcripts).
- ChloeDB and `chloedb-server`: query and content understanding.
  - **Did you mean.** `Db::correct` and `POST /v1/correct` give the words of
    a search that look like typos of a TEXT field's terms, and the text
    with each replaced by the word it most likely meant.
  - **Query expansion.** `bm25(field, :q, expand => n)` ranks by the query
    widened with the terms of its top `n` rows (RM3), read through the
    query's own filter; a coordinator reads them from every shard.
  - **Understanding a search.** `bm25(field, :q, understand => 'boost' |
    'filter', understand_fields => '...')` turns the words that name a
    value of a KEYWORD field (a brand, a category) or a range of a number
    ("under 300") into a boost or a filter, and ranks by the rest.
    `Db::understand` and `POST /v1/understand` say what the words name.
  - **Derived KEYWORD fields.** `KEYWORD OPTIONS (derive = 'body', model =
    ...)` fills a field from a text when a row is written: `builtin:lang`
    (the language), `builtin:keyphrases`, `builtin:minhash` (near-duplicate
    bands), `dict:<table>.<field>` (the values of another table found in the
    text) and `classifier:<name>`. A database with one has the feature
    `derive_keyword`, which 0.2.x refuses by name; one without opens as
    before. `POST /v1/derive/preview` shows what a built-in model makes of a
    text.
  - **Classifiers.** `CREATE [OR REPLACE] CLASSIFIER name ON table(vector)
    LABELS column [OPTIONS (method = 'logistic' | 'centroid', ...)] [WHERE
    ...]` trains on the table's rows and keeps the classifier as a stored
    function `name(v)`: a row's class as a column, a query's class (computed
    before planning, so it works in `WHERE` and boosts), and `DROP
    CLASSIFIER`. `Db::classify`, `POST /v1/classify`, `GET
    /v1/classifiers`. A coordinator trains on every shard's rows.
  - **Chunks.** `SELECT ... FROM chunks_of(table, field, max_tokens => n,
    overlap => m)` splits each row's text at headings, paragraphs and
    sentences; `INSERT INTO t (...) SELECT ... FROM chunks_of(...)` writes
    the chunks to a table.
  - **A table's own synonyms.** `CREATE [OR REPLACE] SYNONYMS ON table AS
    'rules'` and `DROP SYNONYMS ON table`: rules that `synonyms => 1` uses on
    that table after the server's list, kept as a versioned stored function
    that every shard holds. `GET /v1/synonyms` lists them.
  - **Mined synonyms.** `Db::mine_synonyms` and `POST /v1/synonyms/mine`
    propose synonyms from the vectors of a field's words or from one-word
    rewrites in a table of searches. They are proposals only.
  - The command center's Labs > Query understanding explores a search,
    previews what the components make of a row, and configures classifiers
    and synonym lists.
  - Measured (Intel Xeon @ 2.10GHz, 4 cores): expansion raised BEIR nDCG@10
    on NFCorpus from 0.329 to 0.346 and ArguAna from 0.362 to 0.369;
    understanding as a boost raised WANDS from 0.707 to 0.731, and as a
    filter lowered it to 0.631; `builtin:lang` named 96.1% of papluca's 20
    languages right; a classifier named the class of 62.9% of WANDS's
    queries and, as a boost, raised nDCG@10 from 0.433 to 0.439 (p =
    0.002); mined synonyms made no measurable difference on WANDS.
- `chloedb-server`: feedback on the command center. Any view is taken as a
  picture in the browser (a copy of the page drawn onto a canvas, or the
  browser's screen capture), marked up with numbered boxes, arrows, a pen,
  text and black boxes over what must not be kept, and sent with a note
  (`f`, or the speech bubble in the top bar). Each item is a row of table
  `cc_feedback` with the picture, the marks and what was under each on the
  page, the view, and its status and priority; the **Feedback** view lists
  and triages them. `POST /v1/feedback`, `GET /v1/feedback`,
  `GET`/`PATCH`/`DELETE /v1/feedback/:id`, `GET /v1/feedback/:id/shot`, and
  the MCP tools `list_feedback`, `get_feedback` (the screenshot as an
  image) and `update_feedback`, whose `if_status` lets one agent take an
  item.

### Query and indexes

- Text in other languages: 24 analyzers beside `english` and `simple`,
  chosen per field with `OPTIONS(analyzer = '...')`. `standard` reads any
  script: it normalizes (NFKC, lowercase), takes Chinese, Japanese, Thai,
  Lao, Khmer and Burmese words from ICU's dictionaries, makes pairs of
  syllables the terms of Korean, and folds the accents of Latin and Greek
  letters. `cjk` is `standard` with pairs of characters as the terms of
  Chinese and Japanese. 22 languages add what each needs to `standard`: a
  Snowball stemmer for 17 of them (`french`, `german`, `spanish`,
  `arabic`, `russian` and others), a light stemmer for `hindi`, `bengali`
  and `telugu`, and `persian` and `chinese`. On MIRACL's dev queries,
  nDCG@10 from `english` to the best analyzer: Thai 0.289 to 0.457, Korean
  0.262 to 0.374, Japanese 0.048 to 0.295, Chinese 0.000 to 0.197
  ([`results/miracl`](chloedb/results/miracl/README.md), a 4-vCPU cloud
  VM, Intel Xeon @ 2.10GHz). `english` stays the default, and its terms
  and `simple`'s do not change. A database with a field of a new analyzer
  carries the format feature `analyzer:<name>:1`, which a version from
  before this one refuses by name; other databases are written as before
  ([docs](docs/chloedb/index.md#text-in-other-languages)).
- A substring index: a stored `TEXT` or `KEYWORD` column with one value a
  row, declared with `OPTIONS(substring = true)`, answers `LIKE` and
  `ILIKE` with any pattern from an FM-index of its values in each segment
  (the Burrows-Wheeler transform of their bytes in a wavelet tree, with a
  sample of the suffix array), where a pattern with `%` or `_` before its
  end tested every row. The rows are the ones that test gives. On three
  of Loghub's logs a pattern in few rows takes 0.02 to 2 ms where the
  test of every row takes 0.6 to 4 s; the index is about as large as the
  text it indexes, and loading and merging take 1.3 to 3.6 times as long
  ([`results/substring`](chloedb/results/substring/README.md), a 4-core
  cloud VM, Intel Xeon @ 2.10GHz). A database with such a column carries the
  format feature `substring:1`, which a version from before this one
  refuses by name; other databases are written as before. `ESCAPE` is not
  taken on a pattern the index answers
  ([docs](docs/chloedb/query-language.md#substrings-like-and-ilike-anywhere-in-a-value),
  [format](chloedb/notes/substring.md)).
- **Learned sparse fields embedded by a model.** A `SPARSE` field can name a learned sparse model (`"embed": {"from": ["body"], "model": "..."}`; `sp SPARSE OPTIONS(embed = 'body', model = '...')`), as a `VECTOR` field names an embedding model: writes that leave it out get their text's term weights, an `UPDATE` of a source weighs it again, rows stored before the model opened are filled in, and `{"embed": "text", "field": "sp"}` (`Db::embed_query_in`) gives a query's weights from the model's IDF table, with no model run at query time. The model is an ONNX export with an `idf.json` (`chloedb::embed::SparseEncoder`, feature `onnx`; `research/cpu-encoders/sparse_onnx_export.py` writes OpenSearch's doc-v3-distill in int8). Fused with BM25 it came within 1.2 to 1.8 points of nDCG@10 of granite-small-r2's dense vectors on SciFact, NFCorpus and ArguAna, at 50 to 80 times their queries a second ([docs](docs/chloedb/server.md#term-weights-of-a-learned-sparse-model)).
- **ONNX embedding models.** A models directory holding `model.onnx` (an encoder's export, such as ModernBERT's granite-embedding-small-english-r2 in int8) opens as an embedding model (`OnnxEmbedder`, feature `onnx`), with CLS or mean pooling and the query prompt from its sentence-transformers files.
- **Static models in int8, f16 and bf16.** `StaticEmbedder` reads Model2Vec tables stored in int8 (with a scale), f16 or bf16, and Model2Vec's token weights and mapping: potion-retrieval-32M in int8 is 30 MB instead of 129, within 0.002 nDCG@10 on SciFact (0.6315 against 0.6331).

### Static embeddings

- SQL embeds the text it gives where a vector goes. On a field a model
  embeds, `ORDER BY similarity(emb, 'some words') DESC` (or `:q` bound to
  a string, `distance`, `maxsim`, `similarity(emb, embed(:q))`) embeds
  the text as a query by the field's own model when the query is planned:
  in the database from Rust, Python and JavaScript, and on a server's
  embedding cores, as `{"embed": "text"}` parameters are. Before, text
  there was refused. `embed('model', text)` is the query vector of a
  text by a model named, wherever a vector goes; it takes a model's name
  (as `llm()` does, never a path), and a model behind an API only when a
  field uses it. A stored function can no longer be named `embed`.
- `potion-retrieval-32M-int8` and `potion-base-8M-int8`: the Model2Vec
  models with their tables in int8, 31 MB and 7 MB instead of 123 MB and
  29 MB, within 0.004 nDCG@10 of the originals on SciFact, NFCorpus and
  ArguAna. `chloedb-server fetch-model` fetches the pinned original and
  writes the copy; `chloedb-server quantize-model` (and
  `chloedb::embed::quantize_static`) writes any Model2Vec model in int8 or
  F16, the same bytes on every machine.
- The static embedder reads tables in F16, BF16 and I8 besides F32, and
  Model2Vec's `weights` and `mapping` tensors, without which a newer
  Model2Vec model was embedded wrongly.
- The command center's **Models** page (Data): each vector field's model,
  how it makes vectors (static, transformer, late interaction, hosted) and
  how the field is searched and stored, and the models on the server with
  their size, precision and the fields that use them. A field changes
  model or technique without stopping searches: a new field is added
  beside it and filled in, the two are compared on the same words, then
  renamed into place, and the old vectors are dropped (or the switch
  undone). `GET /v1/models` says each model's technique, precision, size
  and fields; `POST /v1/admin/models/fetch` and
  `POST /v1/admin/models/convert` fetch a model, or write a static one's
  table in int8, in the background.

### App engine

- App engine: the app creator is a page of the command center (Apps → App
  creator) rather than a separate studio: describe an app and build it
  there, follow each build (its steps, its screens drawn while the rest is
  written, its timeline), use the app on a phone, a tablet or the whole
  stage, and read, edit and go back through what it is made of. Its views
  run in frames with the studio's sandbox and policy, every read and write
  sent to the page and from there to the engine for that one app. The
  standalone studio stays, for the engine run on its own and as **Studio**.
- App engine: **Made of** says how much of an app's code the engine
  generated from the CREATE TABLE, CREATE SCREEN and CREATE RULE statements
  the agent wrote, how much the agent wrote from scratch, and how much is
  the library's, in lines written and lines that run, piece by piece
  (`GET /api/apps/:id/made`, and `made` in an app's state).
- App engine: apps can be published, open to everyone with no account
  here: **Publish** in the app creator, `/a/<id>` at once, and
  `<name>.<domain>` (or a host of the owner's own) with a domain. The public
  reaches what the app's screens do and nothing of how it is made, at a
  rate a minute per visitor. The operator's `spec.apps.publish` (`domain`:
  your own or `sslip`) adds a load balancer and Caddy for the apps'
  certificates.
- App engine: **Usage** in the app creator is the studio's Activity view:
  each visit step by step, and the paths people take between screens.
- App engine: `GET /api/events?after=N` gives the studio's live events to a
  page that cannot hold a stream open (chloedb-server forwards whole
  answers), and `GET /api/kit` the frames' runtime and policy.
- App engine: a published app stays open when a read of the published apps
  fails (it kept closing every one, answering 401, while the cluster was
  slow), and a visitor can do less: a visitor's `/error`, or a function
  that fails on a visitor's call, starts no repair; `/people` gives
  visitors names and roles but no emails; a visitor's `/fetch` takes none
  of their options and adds no host to the owner's list, and `/ask` takes
  only `json`; a public sign-up is a member, never the owner.

### Experimental

- Tenants: a database kept whole in an object store (a directory, or S3
  and the stores that speak its API), committed by replacing one `HEAD`
  object on the condition that it is unchanged, so that of two nodes
  writing one tenant the store refuses the later. `chloedb::tenant` in
  the engine, and `chloedb-server --tenants URL` with admin routes under
  `/v1/admin/tenants`. It is a first slice of the 0.7.0 design
  ([docs/design/0.7.0-file-per-agent.md](docs/design/0.7.0-file-per-agent.md)),
  off unless asked for, and its layout (`layout: 0`) may change without a
  migration ([docs/chloedb/tenants.md](docs/chloedb/tenants.md)).
  Writes of one handle that wait for a commit share the next one; a
  handle takes its working copy up again after a restart; the server
  closes idle tenants (`--tenant-idle-s`, `--tenant-open-max`), lists
  them (`GET /v1/admin/tenants`), and has `chloedb-server tenants
  check-store URL` and `tenants list URL`.
- A blackboard for agents, on a tenant: tasks claimed under leases,
  findings kept once, a timeline, and calls applied once when sent again
  with their key. `chloedb::blackboard` in the engine; on the server a
  tenant made with `"template": "blackboard"`, its tools at
  `POST /v1/admin/tenants/:id/board/:tool` and as an MCP server at
  `POST /v1/admin/tenants/:id/mcp?agent=NAME`
  ([docs/chloedb/tenants.md](docs/chloedb/tenants.md#a-blackboard-for-agents)).
- `chloedb::storage::ObjectStore` has conditional writes, whole-object
  reads and listing (`put_if_absent`, `put_if_version`, `get`, `list`,
  `list_dirs`, `get_file`), on the directory store and on S3. A store written outside
  this repository keeps compiling: the new methods default to an error.

### Fixed

- `chloedb-server`: its own reads (a classifier's training rows, the rows
  `chunks_of` splits) are not held to `--max-rows`, and a query that
  computes a classifier or `llm()` per row reads no more rows than
  `--max-rows`.

## 0.2.1

Safety fixes from the audit of 12 November 2026, with no change to the file
format: `chloedb-server-v0.2.1`, `chloedb-py-v0.2.1` and
`chloedb-js-v0.2.1` (the Java package and ChloeCtx are built from source).
It has breaking changes, each marked below. The shard protocol stays 20:
`RestoreChunk` and `VerifyRequest` gain a `config` field, which a server of
0.2.0 does not send and this one accepts as 0, so a shard can run both
versions during an upgrade. It also brings the one-line installer, the
app engine's schedules, sources and reach controls, and a first set of
additions, none of which changes the file format or the shard protocol:
`PACK n TOKENS`, set operators on multi-valued fields, ChloeCtx capsules,
the Python framework adapters and options for training semantic IDs.

### Added

- ChloeDB: `PACK n TOKENS` ends a SELECT and fills a budget of tokens
  rather than a number of rows: rows too long are cut to the stretch that
  matches the query (`EXCERPT n`), rows that repeat one taken are skipped
  (`DEDUPE x`, by words or `ON` a vector field), rows an `EDGES` field
  links to come with the row that links them (`FOLLOW links 1 HOP`), and
  `WITH RECEIPT` says why each candidate was taken or skipped. `Db::pack`
  answers the receipt with the snapshot read. On a file, a server and a
  coordinator; no change to the file format or the shard protocol.
- ChloeDB: set operators on a multi-valued field, as PostgreSQL's arrays
  have them: `tags @> ARRAY['red', 'sale']` (every value of the list),
  `tags && ARRAY[...]` (any of them) and, on an `INT[]` field,
  `groups <@ :allowed` (each value of the row is in the list), read from
  the field's index and usable as a search's filter. A `KEYWORD[]` field
  does not take `<@` yet.
- ChloeCtx capsules: `chloectx pack DIR` writes a slice of a project's
  memory (decisions, rules, facts, open tasks, notes, their entities and
  the conversation, redacted again, without raw tool calls) to one
  read-only file and a manifest with its hash, and `chloectx mount DIR`
  serves it to another agent's Claude Code as the MCP tools
  `capsule_ask`, `capsule_search`, `capsule_timeline` and `capsule_sql`.
  With a short handoff note the next agent solved 72% of the follow-up
  tasks, and 89% with a capsule as well
  ([docs/chloectx/capsules.md](docs/chloectx/capsules.md)).
- Python: `chloedb.langchain` (a LangChain vector store),
  `chloedb.langgraph` (a LangGraph checkpointer and store),
  `chloedb.llama_index` (a LlamaIndex vector store and embedding) and
  `chloedb.dbapi`, a DB-API 2.0 driver, with the extras `langchain`,
  `langgraph` and `llama-index`
  ([docs/chloedb/frameworks.md](docs/chloedb/frameworks.md)).
- In the repository: `chloedb-swarm`, simulated users who do tasks with
  ChloeDB through its MCP tools or the command center, and a review of
  each session ([chloedb-swarm/README.md](chloedb-swarm/README.md)). It
  ships in no package.

### Security

- `chloedb-server`: a key rotated on one server ends on every server of
  its cluster. Merging keys took only the time a key was last used, so the
  old secret of a key rotated on a coordinator stayed valid on the shards,
  on other coordinators, and on that coordinator once it started again and
  took its keys back from the shards. A key's end is now the earlier of
  the two a merge sees.
- The OpenSearch listener (`--opensearch-listen`) inflates a gzip body to
  `--max-body-mb` at most (413 past it), asks a read key for every route
  but `/` and `_cluster/health` (a feed key read documents before), and
  keeps refusals and changes of indexes, mappings, settings and aliases
  in the audit log, as the main listener does.
- `POST /v1/alerts/peer`, by which coordinators tell each other their
  alerts, takes the cluster's token (or the server's) only: an admin key
  could replace rules, routes and silences without an audit entry.
- Every HTTP answer carries `X-Content-Type-Options: nosniff`,
  `X-Frame-Options: SAMEORIGIN`, `Referrer-Policy: same-origin` and a
  `Content-Security-Policy` with `frame-ancestors 'self'`, unless its
  handler sets its own, so that another site cannot frame the command
  center.
- The HTTP listeners close a connection that takes longer than
  `--header-timeout-secs` (30) to send a request's headers or a TLS
  handshake, or that stays idle as long, and hold `--max-connections`
  at once (by default three quarters of the open-file limit): a client
  sending a byte a minute on thousands of connections no longer holds
  the process's file descriptors.
- A server over TLS that is told to stop finishes the requests under
  way, as one without TLS did, instead of cutting them; both wait
  `--stop-grace-ms` (20 s) at most, and a follower's open feed ends when
  the server stops taking connections, rather than holding it up until
  it is killed.
- **Breaking:** `follow-postgres` with no `sslmode` in its URL requires TLS
  of a database on another machine (`prefer` stays the default for
  `localhost` and loopback addresses), and warns when a URL asks for
  `prefer` or `disable` of one.
- The image `ghcr.io/chloetech-ai/chloedb-server` has a user 10001
  (`chloedb`) that owns its `/data`; run it as that user
  (`docker run --user 10001:10001`). Its own user stays root for now, so
  that an operator installed before 0.2.1, which runs new images on volumes
  a root image wrote and gives its pods no user, keeps working. The
  operator of 0.2.1 runs the servers, the coordinators and the pre-flight
  as user 10001 itself and gives the shards' volumes to its group
  (`fsGroup`). A Docker volume a root image wrote needs a `chown` once
  ([the server's page](docs/chloedb/server.md)). The image is built from
  base images pinned by digest and holds neither the benchmark tools nor
  `iproute2` (`docker build --target bench` makes an image with
  `chloedb-server-bench` and `engine-compare`). Images carry build
  provenance.
- ChloeCtx: **breaking:** `chloectx app` refuses to start on an address
  other than loopback (`--host 0.0.0.0`, say) with no `--token`, since
  whoever reached the port could read and write every space and start
  model calls billed to the app's key; `--insecure` starts it anyway. The
  operator passes `--insecure` for a cluster without a `tokenSecret`
  (which is open itself), and the app's token otherwise, as before. The
  app also serves 64 requests at once (one more is answered 503) and runs
  at most four distillations, with 256 waiting; past that an exchange is
  stored without notes and `/v1/info` counts it under `distill.skipped`.
  Before, each request and each exchange started a thread, without bound.
- ChloeCtx: **breaking:** `chloectx sync` refuses a server reached over
  plain `http://` anywhere but this machine (`127.0.0.1`, `::1`,
  `localhost`), since it sends transcripts and the server's key;
  `CHLOECTX_SYNC_ALLOW_HTTP=1` allows it. Each request's header file (the
  key, owner-only) is its own, so a push every 30 s and an explicit
  `chloectx sync` no longer remove each other's.
- ChloeCtx: the inspector (`chloectx ui`) is served with a
  Content-Security-Policy that runs only its own script (by hash), loads
  nothing from elsewhere and connects only to itself, and with
  `X-Frame-Options: DENY`.
- `install.sh` builds ChloeCtx with `--locked`: the dependency versions in
  `chloectx/Cargo.lock`, the ones CI tested, not newer ones the registry
  offers on the day.
- `chloedb-sheets`: a SQLite file's views are listed but not counted, and
  come into a workbook only when named (the UI leaves them unticked): a
  view runs its query as it is read, and a file's can be a query that
  never ends, which held a server thread for good. Reading a SQLite file
  stops after 60 s for its list of tables and 180 s for an import, and the
  file is opened with `trusted_schema` off. An xlsx file is refused when
  its parts expand past 512 MB (counted by decompressing them, not by the
  sizes the file declares) or it holds more than 2 million cells, as CSV
  and SQLite imports were bounded already.
- App engine: `app.sql` keeps a view to its app's tables. It reads the SQL
  as ChloeDB's parser does, finds every table it names (a quoted name, a
  name after a comment and a CTE's name outside its query were not
  checked), refuses block comments, prefixed and `$$` strings, quoted or
  qualified table names, `|>`, `INTO` and `TABLE`, and then asks the
  server's plan (`/v1/explain`) which tables the query reads.
- App engine: a view's frame takes images and media from the engine and
  the photo hosts only (and, once the owner turns on "Web pages and APIs",
  from the hosts allowed there to read from), submits no form and opens no
  popup; its links open from the studio after a click. A frame that
  navigates away is closed.
- Crawler: it reaches the public internet only. An address on its
  machine, a private network, a link-local range (cloud metadata) or
  another reserved one is refused, linked, redirected to or resolved to,
  unless it is started with `--allow-host HOST[:PORT]` or
  `--allow-private`. Each redirect is checked as a link is (scope,
  `include`, `exclude`, robots.txt); robots.txt is read to 500 KB and a
  sitemap to 50 MB.
- App proxy: a path under `/v1/apps/<name>/` (and the hub's `/c/<name>/`)
  stays under the app's URL: a dot segment, once or twice encoded, is
  refused, and a decoded `?` or `#` stays in its segment. `/v1/apps` asks
  the apps at once and keeps a failure for 15 s, so an app that does not
  answer no longer empties the command center's list.

### Cluster and operator

- A copy that was removed from a shard while it was down can no longer
  come back and, with other removed copies, elect itself and fence the
  live primary: a primary or replica refuses Replicate, Restore and Verify
  from a primary whose membership version is older than its own (the
  shard protocol's new `config` fields).
- **Breaking:** a coordinator refuses (409) a write to a shard whose
  servers have no roles, which it used to write copy by copy so that the
  copies drifted apart; `--write-every-copy` writes each one as before.
- Coordinators run by the operator change the cluster map only while they
  hold the Lease `<cluster>-map`, and write the map in doubt into it before
  each change, so two coordinators no longer change the map at once. The
  operator's ClusterRole (`deploy/k8s/operator/operator.yaml`) gains
  `coordination.k8s.io` leases; an operator whose ClusterRole is older
  makes the coordinators' Role without the Lease and logs a warning, and
  its coordinators change the map as before. Apply `operator.yaml` again
  to turn the Lease on.
- `autoscale.volumes` has a ceiling: `maxSize` (default 1Ti) and
  `maxShards` (default 16), and the rule does not fire again until its last
  change shows in the reading.
- Cloud setup: a cluster is created only with `keep: true` or
  `delete_after_hours`, and a plan has at most 100 nodes. EKS nodes run in
  private subnets behind one NAT gateway by default (`public_nodes` opts
  out), `api_cidrs` limits the API endpoint, and spot with fewer than 3
  copies warns.

### Install

- `curl -fsSL https://chloetech-ai.github.io/chloedb/install.sh | sh`
  installs `chloedb-server` from the newest release into `~/.local/bin`,
  checked against its `.sha256` (Linux x86_64 and aarch64 with glibc
  2.35+, macOS arm64 and x86_64).
- A Homebrew formula is built and tested for each release and attached to
  it; it goes to a tap only once the tap is configured.
- The Python package's Linux wheels are built for manylinux_2_28 (glibc
  2.28), and a release can upload them to PyPI once trusted publishing is
  set up.
- The image runs `--db /data/chloedb.chloedb --listen 0.0.0.0:7700 --grpc
  0.0.0.0:7701` when given no arguments (it still needs `CHLOEDB_TOKEN`).
- [docs/quickstart.md](docs/quickstart.md): from install to a first search.

### App engine

- An app reaches only the hosts its owner allowed, each read-only or
  allowed to send, checked at every redirect; API keys are sealed
  (AES-256-GCM), bound to hosts and never seen by the app's code. Apps that
  already had fetch on and no list keep reaching any host.
- `CREATE RULE ... EVERY ...` runs rules on a schedule, `CREATE SOURCE`
  reads RSS, Atom or JSON feeds into a table on a schedule, and `PULL`
  does the same read inside a rule.

### Storage

- A SPARSE value whose term holds the character `\0` is refused when it
  is written. It was accepted, and the next flush panicked building the
  field's dictionary: on the flusher thread, which ended for good, and
  again at each reopen, which replays the row. A term of that kind in a
  row written before is left out of the index.
- A panic in background work is that round's error, tried again as an
  error is, and no longer ends the thread: the build of a background
  flush, a merge on a merger thread, the cold tier's offload and eviction,
  and a read-only database's refresh.
- A checkpoint (a backup, the copy a replica is seeded from) of a database
  where a column references a table created after its own table
  (`ALTER TABLE .. ADD COLUMN .. REFERENCES`) failed with "references ..,
  which is not a table". It copies such a database.
- Semantic IDs: a background flush keeps the tie numbers of rows deleted
  before it. It dropped them with the rows, so the next row under the
  same levels was given a deleted row's whole code again: a code named
  two rows over time, which the tie exists to prevent. A foreground flush
  and a merge already kept them.

### Query and indexes

- **Breaking:** on the operators engine, a `LIMIT` over a join, `DISTINCT
  ON` or `MAX k PER` whose first table holds more than 1,000,000 rows is
  an error when the rows it wants are not among the first 1,000,000 read,
  as the same query without its `LIMIT` is. It answered a short page,
  down to no rows at all, without a word.
- A plan from elsewhere (`Db::run`, `POST /v1/plan`) with a NOT over a NOT
  or over `!=` matched the rows whose field is NULL: `not(not(price > 5))`
  returned rows without a price. Each NOT is SQL's now, as in SQL text,
  which was not affected.
- The filter of `json_paths(.., filter => ..)`, `Db::json_paths` and
  `Db::sid_next` is one condition, parsed by itself. Its text was put
  into a statement's, so `'x = 1 LIMIT 0'` was taken as a filter.
- Semantic IDs: `chloedb.train.codebook(balanced=True)` (Rust's
  `TrainOptions.balance`, JavaScript's and Java's `balanced`) trains each
  level of a codebook with every centroid given the same number of the
  training vectors. Plain k-means uses the later levels' numbers
  unevenly; on Amazon Beauty's 12,101 items, balanced, the third level's
  256 numbers are all in use alike and the items sharing a code go from
  3,288 to 2,601. Off by default, and a codebook's file is as before.
  `chloedb.train.codebook_report` (`Codebook::report`,
  `Db::codebook_report`) says what a codebook makes of a set of vectors:
  the distinct codes, the rows sharing one, and each level's use.
- Sequence models: `chloedb.train.sequence(logq=True, uniform_negatives=n)`
  for catalogs too large to train against every item. With the other
  targets of a batch as the negatives the model learns to rank popular
  items low; `logq` takes that out of each negative's score and
  `uniform_negatives` adds items drawn evenly. On Yambda-50M's 629,298
  tracks, held-out Recall@10 after four epochs goes from 0.0229 to
  0.0334. Both are off by default.
- CI's recommender gate checks semantic IDs: a plain and a balanced
  codebook on MovieLens-1M, the codes a table gives against
  `codebook_report`, and the report against the fixtures.

### Replication

- **Breaking:** a database that numbers its writes (`--seq-no`) refuses a
  bulk load (`Db::load`, `Database::bulk_load`) and `attach_segments` once
  it has numbered a write or replicates: they write no log record, so its
  replicas never got the rows. Before that they still seed it. Imports
  (`POST /v1/import`, `chloedb-server import`, SQLite, Parquet and Arrow)
  into such a database upsert in transactions of 10,000 rows, also into an
  empty table: a bad row then stops the import after the transactions
  before it, and a key the file holds twice keeps its last row.
- `chloedb-server follow` and `follow-postgres` no longer stop with
  "table ... already exists" when a `CREATE TABLE` or `ALTER TABLE` they
  sent was applied but its answer was lost and they sent it again: a
  table found as the source has it is taken as made.

## 0.2.0

The first release under the new names: `chloedb-server-v0.2.0`,
`chloedb-py-v0.2.0` and `chloedb-js-v0.2.0` (the Java package and ChloeCtx
are built from source). It has breaking changes, each marked below: a
memory budget on embedded queries, writes that need a model which is not
open, short pages past a fused ranking's candidates, model endpoints'
`key_env` names, and stored SVG and HTML served as downloads. The shard
protocol is 20; servers of protocols 18 and 19 still talk to it.

### Security

- `chloedb-server`: media fetches (`--media-fetch`) check the host the
  HTTP client connects to. The allowlist is compared with the URL as the
  client parses it, before the request and at each redirect, and a URL
  that URL parsers could read differently is refused: one with a
  backslash or a control character (a tab, a newline), a host with
  characters other than letters, digits, `-`, `_` and `.` (percent-encoded
  or not in its `xn--` form), or a host ending in a number that is not a
  plain IPv4 address. Before, a wildcard entry (`*.example.com`) could be
  passed by a URL whose host the server fetched from was not under it.
- `llm()` takes a model's name, never a path: letters, digits, `_`, `-`
  and `.`, two such names joined by `/` at most, as `cross_encoder()`
  takes. An absolute name or `..` is refused, so a query reads no
  `endpoint.json` outside the models directory.
- **Breaking:** an `endpoint.json`'s `key_env` names a provider's key
  variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, ...) or one whose name
  starts with `CHLOEDB_MODEL_KEY_`; any other name is refused when the
  model is opened, so an endpoint is never given another of the server's
  variables (its tokens, a cloud's credentials). Rename a variable such
  as `GPU_BOX_KEY` to `CHLOEDB_MODEL_KEY_GPU_BOX`.
- `GET /v1/media/<sha256>` serves a stored file so that a browser never
  runs it on the server's origin: images (PNG, JPEG, GIF, WebP, BMP,
  AVIF), sounds and videos with their type; a PDF as a download; any
  other file, whatever type its writer gave (HTML, SVG, XML, JavaScript),
  as a download of `application/octet-stream`; every answer with
  `X-Content-Type-Options: nosniff` and
  `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; sandbox`.
  An SVG in an image field is no longer shown inline by the command
  center's media views.
- ChloeCtx: `memory_sql` and the inspector's query box run one parsed
  `SELECT`, not any text that begins with `select`, and refuse `llm()` and
  `cross_encoder()`, which send the rows they read to a model with the
  daemon's keys. Each runs with a deadline (2 s for the tool, 10 s for the
  inspector), a memory budget and a row cap (1,000 and 50,000). The query
  engine's model-written conditions refuse the same functions.
- ChloeCtx: the distiller's `claude -p` runs in `distill/` in the stores'
  folder, private to the user, instead of `/tmp/chloectx-distill`, from
  which another local user's project settings (a hook) would have run as
  this one. Its answer is read while it is written, so one over 64 KB no
  longer blocks the call until its timeout.
- ChloeCtx: the short socket path is used only in a directory of the
  user's own that no one else can open: a `/tmp/engram-<uid>` another user
  made first, or a link, is skipped for `~/.cache/chloectx`.
- ChloeCtx: `fetch-model` downloads the commit ChloeDB pins and checks each
  file's SHA-256 before it is renamed into place; a download cut short is
  fetched again by the next `install.sh`, which now always runs it.
- ChloeCtx: `install` quotes the binary's path in hook commands (a path
  with a space ran something else), and `doctor` runs the program as the
  hook command spells it. Settings files are written beside and renamed
  into place, keep their keys' order and their links, and a project
  install backs up the files it changes.

### Storage

- Opening no longer falls back silently past a damaged newest version (a
  damaged meta slot or manifest, or a file cut short) when that would lose
  acknowledged writes: it fails with an error naming the file, the damage,
  what would be lost and how to recover, and changes no file. A log whose
  header is damaged but whose commits are intact, or a log from another
  database, is refused the same way, and so is a first page of zeros
  (slot 0 is written when a database is created, so zeros there are
  damage), and so is a zeroed slot 1 where the file shows a commit wrote
  it: slot 0 holds a version past the one the database was created with,
  or the log starts past the transactions after that version, or the file
  has pages past it and no log, as a checkpoint has until it is opened.
  A newer version that changed tables (created, altered or dropped one)
  or began numbering writes is never taken for one the log covers, since
  none of that is logged: falling back past it lost the table and the rows
  logged into it. Before, one flipped byte could reopen an older version
  without a word.
- Without `sync`, a log whose header is zeros before intact commits is
  what a power loss leaves when the header never reached the disk: it is
  dropped (copied to `<wal>.damaged`, and `damaged()` says so), as a power
  loss without sync may, instead of being refused as damaged. With `sync`
  the header is durable before any record, so zeros there are refused.
- `recover` (Rust `DbOptions`, Python `chloedb.open(..., recover=True)`,
  `chloedb-server --recover`) accepts the loss on purpose: it keeps a copy
  of the damaged file as `<path>.damaged-<time>` first, and `damaged()`
  (Python `Database.damaged`, with a `RuntimeWarning`) says what was lost.
  A server opened that way answers `degraded` on `/v1/health`, and
  `/v1/admin/check` fails.
- A damaged column fails the query that reads it instead of reading as
  NULL; a checkpoint refuses a segment whose checksum is wrong instead of
  copying it with a fresh one.
- A failed sync of the database file poisons the handle, as a failed log
  append already did, so no later commit or merge can claim pages that
  never reached the disk.
- On Linux the writer's lock is held on the file itself, so a symbolic
  link, a hard link or a deleted `-readers` directory no longer lets a
  second writer open it. Not on macOS, where `flock` and `fcntl` locks
  are one kind and the readers' shared lock would refuse it: there the
  writer is still the one holding `-readers/WRITER`.
- `close()` flushes, so after a clean close the file alone is a complete
  copy of the database. Python adds `flush()` and `checkpoint(dest)`;
  docs/chloedb/python.md and index.md say how to back a database up.
- A write-ahead log deleted outright while it held acknowledged
  transactions is refused at open (read-only too), naming the log, the
  transactions committed after the version on disk and `recover`, where
  the database used to open at that version without a word. The file is
  not written between versions, so the writer keeps a log record at byte
  24 of `<path>-readers/WRITER` (`WRITER` in the directory layout): the
  database's id and the version's txn, written and synced before the
  first append after a version, after a version made while the log holds
  transactions it does not, and at an open that replays a log. Versions
  before it read and write only the first 24 bytes, so it is compatible
  both ways. Only a missing log file beside no sealed log is refused; a
  clean close empties the log, so deleting it afterwards still opens, and
  so does a checkpoint renamed into place (its database id is its own).
  `recover` opens the version on disk, `damaged()` says what was lost, and
  the record is cleared. A tool that deletes a log on purpose deletes
  `-readers` with it (`chloectx-bench` does).
- Not yet: a log deleted together with `-readers` (or `WRITER`), a copy of
  the file opened elsewhere without its log, or a log deleted while a
  background flush holds a sealed one, cannot be detected; nor can a
  zeroed slot 1 that held the only version made since creation beside an
  empty log, which looks like a crash during that first commit; there is
  no point-in-time recovery.
- A damaged freelist whose counts or extents overflow when added up is
  refused as malformed; decoding it panicked before (found by the nightly
  fuzzer).

### Query and indexes

- A ranked query that stops at the default 100 rows says so: its stats
  carry `implicit_limit: 100` and an exact `truncated` (the engine ranks one
  more row to know), a plan line says it, and the Python and JavaScript
  results expose both. `DELETE ... ORDER BY <score>` without `LIMIT` is
  refused: before, it deleted the top 100 and left the rest.
- Fused rankings (`rrf`, weighted sums): the candidates follow the page's
  `LIMIT`, not its `OFFSET`, so pages of one size are pages of one ranking
  and no row repeats or goes missing between them. A page beyond the
  candidates comes back short, flagged `truncated`; raise `depth` (or use
  `exact => 1`) to page further. This replaces the deeper re-ranking of
  later pages, which made pages disagree (up to 85 of 220 rows repeated
  in the audit's test; none now).
- `ppr` gives the same scores when every edge weight is scaled alike, so
  counts work as weights (weights of 1,000 used to return nothing at the
  default `eps`); `max_pushes_reached` is reported when a walk stops early.
  Its rankings move on MovieLens-1M (the recommender gate's fixtures):
  Recall@100 0.568 to 0.577 and coverage@10 0.528 to 0.475 leaving one
  out, Recall@10 0.059 to 0.062 on the time split, where it now beats
  both baselines.
- Vectors: an unfiltered segment small enough to scan is searched exactly,
  the cost model scales with dimension, and the default `ef` is 200
  (was 64): on 20,000 random 64-dimension vectors the default now finds
  the true top 10 (was 73%), on 200,000 it finds 71% (was 40%). `distance()`
  is now the Euclidean distance (it was the dot product, reversed); a plan
  that uses it names the plan feature `distance`, which older shards refuse.
- Models: the known models are fetched at a pinned Hugging Face revision and
  each file's SHA-256 is checked; a table created with a model records a
  fingerprint of its weights and refuses different weights under the same
  name; a write that needs a model which is not open fails (it used to store
  the row without a vector) unless `set_embed_later(true)` says to fill the
  vectors in later, which chloedb-server does. This holds for every write
  path: SQL, `load`, Python and JavaScript inserts, and the Arrow, SQLite
  and file imports. **Breaking for embedded use:** code that wrote rows
  without opening its model (or before the model was downloaded) now gets
  an error; open the model (`models=DIR`), give the vectors, or call
  `set_embed_later(True)` and run `embed_backfill` once the model is there.
- Not yet: a per-field metric option (cosine, dot, L2), and a fingerprint
  recorded on a model's first open for tables created before this change.
- A database opened in a program (Rust's `Db`, and the Python, JavaScript
  and Java packages) counts its queries against a memory budget the whole
  process shares: a quarter of the machine's memory (its cgroup limit, else
  RAM), or `CHLOEDB_QUERY_MEMORY_MB` MiB (0: none). A query past it fails
  with a `memory:` error naming the budget and how to raise it, and gives
  back what it held, where it used to grow until the process ran out of
  memory (`SELECT *` of a large table, a `GROUP BY` with a group per row, a
  percentile over every row). Such a database still sets no deadline, row
  window or group cap: a server's are for answers sent over the network,
  and a program reading a whole table into a frame or a scroll has no way
  past them; `max_rows` and `max_groups` stay options. `ExecOptions::limited`
  gives one query a deadline, a budget of its own and a cancel flag,
  `Interrupts` cancels the queries running from another thread, and
  `Db::sql_with`, `Db::run_with` and `Db::query_leaf_with` take options as
  `Db::query_with` does. chloedb-server keeps its own budget
  (`--query-memory-mb`), whose refusals now name that flag. **Breaking for
  embedded use:** a query that needed more than a quarter of the machine's
  memory now fails; set `CHLOEDB_QUERY_MEMORY_MB` (0 for no budget), give
  the database or the query a larger `memory`, or `db.exec.memory = None` in
  Rust.

### Replication

- A shard's copies change one at a time, and only by a primary that a
  majority of its current copies answered within its lease and that a
  majority already follows (it waits up to 3 s for that before refusing
  with 503). Before, an admin call could give a primary cut off from its
  majority a new set of copies while the other side elected its own
  primary, and both acknowledged writes. `accept_data_loss` (in
  `Configure` and on both admin routes) overrides the check on purpose and
  is logged as an error. Memberships are versioned and sent with every
  `Replicate`; a voter refuses a candidate whose membership is older than
  its own.
- A new primary commits a no-op transaction of its own term before it
  acknowledges anything, so it never acknowledges an older term's
  transaction by counting copies (Raft's "figure 8"). The simulator has a
  new bug to find, `acks-an-older-term`.
- A write remembers the term its primary took it in, and is refused
  unapplied, answered "in doubt" (409), once the database has left that
  term; the check is repeated under the writer's lock, which shipped
  transactions also take. Before, a write stalled during a handover could
  commit after the new primary's first transaction with the same term and
  sequence number as a different transaction, leaving copies apart.
- Writes through a coordinator that span shards are checked on every
  shard first (protocol 19, `WriteRequest.validate`): an insert of an
  existing key, a failed `if_seq_no`, a type or NOT NULL error on one
  shard now fails the whole write with nothing applied, naming the
  operation. A shard whose writer stays busy for 100 ms, or that holds a
  newer slot map, is not checked, and its part goes straight to its write
  queue. A race remains between the check and the apply; see "When a
  write fails" in docs/chloedb/server.md.
- Every write and statement through a coordinator carries an idempotency
  key from its first send, so the coordinator's own resends never apply
  it twice; refused writes carry an `x-chloedb-write` code (`not-applied`,
  `in-doubt`) that the coordinator reads instead of the error's words;
  `--idempotency-ttl-s` sets how long keys are kept.
- A copy holding a different transaction than its primary's under the
  same term and sequence number is found on the `Replicate` that follows
  it (protocol 20, `ReplicateRequest.prev_digest`): each call carries a
  digest of the transaction it follows, the replica compares its own,
  and on a mismatch applies nothing, serves no reads, logs an error and
  says `differs` (health `degraded`, `txn_mismatches`,
  `chloedb_replica_txn_mismatches_total`); the primary sends it a copy of
  itself, which drops its own. A replica's word that it holds a run of
  the primary's history no longer counts toward a write's acknowledgment
  until a call that follows it applies, and that call always carries the
  primary's digest (a log that no longer holds the transaction sends a
  copy instead). Peers of an older protocol apply
  by term and number as before. A replica found to differ keeps that
  beside its database (`differs` in `<db>.replication`), refuses calls
  and campaigns until it installs a copy. The simulator has two bugs to
  put in, `commits-after-stepping-down` (a stalled writer) and
  `trusts-the-same-term`.
- A copy decided for a replica is always sent: a shipper whose primary was
  given other replicas between deciding a copy and sending it left the
  replica marked as being sent one, and the replica's next shipper waited
  for that copy for ever; and a copy that fails after the primary was
  given other replicas is sent again by the next shipper, rather than
  shipping batches to a replica that holds none of the primary's rows.
- Not yet: the digest covers the one transaction a call follows, not the
  history before it, and only while the replica's ship log holds it (a
  replica restarted after a flush, or copied since, before it was found,
  and a replica whose last is a primary's last that its log does not
  hold, are left to the per-slot digest round, `--verify-every-s`); a
  replica ahead of its primary under the primary's own term is still left
  to an operator; writes across shards are not atomic (no two-phase
  commit).

### Clients (Python, JavaScript)

- Writes through a client answer their sequence numbers. In Python a
  write's count is a `chloedb.Written`, an `int` with `seq_no` (a server's)
  and `seq_nos` (a coordinator's, one per shard); in JavaScript
  `client.writeResult(ops)` answers `{written, seqNo, seqNos, skipped}`,
  and `write` still answers a number.
- Reads take `min_seq_no` / `min_seq_nos` (`minSeqNo` / `minSeqNos` in
  JavaScript): `query`, `query_arrow`, `search`, `get`, `get_many`,
  `scroll`, a read by `sql`, a read plan by `run`, `sid_next`.
- `client.session()`, a client whose reads see its own writes: it sends
  the latest numbers of its writes with each read, through a coordinator
  or from a replica.
- Errors have a stable `code` (`bad_request`, `not_found`, `conflict`,
  `partial_write`, `busy`, `overloaded`, `timeout`, `unavailable`, ...),
  from the server's JSON `code` when it gives one, and the common codes
  are raised as subclasses of `ChloeDBError` (`ConflictError`,
  `BusyError`, ...); `except ChloeDBError` and `instanceof ChloeDBError`
  still catch every one. In-process Python errors carry the same codes.
- A Python `Database` raises `TypeError` for a client's options
  (`timeout_ms`, `idempotency_key`, `keep_alive_ms`) instead of binding
  them as query parameters, and takes `min_seq_no(s)` as no-ops, so code
  moved from `chloedb.connect` to `chloedb.open` fails loudly rather than
  losing its deadline or idempotency. A query parameter of such a name is
  passed in `params`.
- A database in the process takes query limits, and can be stopped from
  another thread. Python: `timeout_ms` (a deadline, as a `Client`'s query
  takes it, raising `DeadlineError`) and `memory` (a budget in bytes; 0:
  none) on `query`, `query_arrow`, `scroll`, `sql`, `search` and `run`, the
  same on `chloedb.open`, `Database.open`/`create` and `set_limits` for
  every query of the database, and `Database.interrupt()`, as
  `sqlite3.Connection.interrupt`. JavaScript: `{timeoutMs, memory, signal}`
  (an `AbortSignal`) on `query`, `sql`, `run` and `search`, `timeoutMs` and
  `memory` on `open` and `setLimits`, and `db.interrupt()`. Java:
  `QueryOptions` on `query`, `execute` and `SearchOptions.limits`,
  `Database.Options.timeoutMs`/`memory`, `setLimits` and `interrupt()`; the
  JDBC driver's `Statement.setQueryTimeout` now holds (it was kept and
  ignored) and `Statement.cancel()` works (it threw). `timeout_ms` to a
  Python `Database` is no longer the `TypeError` of the change above except
  where nothing runs a query (`plan`). The HTTP clients are unchanged, but
  for a `search` that passes `timeout_ms`/`timeoutMs` on to their `query`.
  **Breaking:** a Python query parameter passed as the keyword `memory` is
  now the query's budget, not a parameter; pass it in `params`.

### ChloeCtx

- `chloectx sync` keeps a project's memory on a ChloeDB server or cluster
  as well as in its local store: the daemon pushes what it records every
  30 s and as it exits, and when it starts it pulls the records the store
  lacks, so a fresh cloud container or another machine recalls what
  earlier sessions stored. `sync setup --url URL` with the key from stdin
  (`--token-stdin`) or a variable (`--token-env`); `status`, `push`,
  `pull`. See "Keep the memory on a ChloeDB server" in
  docs/chloectx/setup.md.
- Redaction catches the secrets it used to miss: `DB_PASSWORD=…` and
  other names that end in a word for a secret (in env dumps, `.env`
  files, YAML and JSON), passwords in URLs and `curl -u`, `Authorization`
  headers, JWTs, and the keys of Stripe, AWS, Slack, Hugging Face,
  DigitalOcean, npm, GitLab, Google and ChloeDB itself. Each is replaced
  by a marker naming its kind (`[REDACTED:password]`, where it was
  `[REDACTED]`), on every path into the store (prompts kept in
  `state.json` and `events.log` too) and again as `chloectx sync` sends
  and pulls. `chloectx redact [--all] [--dry-run]` rewrites a store
  recorded before: its records, session state and log, the sync server's
  copies, and the file itself, so the old versions leave its free pages.
  See "Secrets and `chloectx redact`" in docs/chloectx/setup.md.

### Build and release

- GitHub Actions pinned to commits (Dependabot keeps them current); every
  workflow has a least-rights `permissions:` block; cargo, maturin and the
  JS addon build with `--locked`; the Python and JavaScript releases attach
  a `SHA256SUMS`.
- [docs/compatibility.md](docs/compatibility.md): the file formats this
  code reads and writes, the shard protocol, and what a version promises.
- A release tag builds and publishes nothing unless its commit is on main
  and CI's whole matrix (the nightly run, or one started by hand) passed
  on it (`release-gate.yml`); the release workflows check that the binary
  starts and leave the tests to CI. The server's image publishes `latest`
  only after CI passed on this repository's main, and `<version>` from a
  release tag.
- A nightly run that fails opens an issue (`nightly-failure`); the fuzz
  targets build again and run every night on a kept corpus.
- Dependabot watches every lockfile, and `cargo deny` checks advisories
  and licenses daily (`deny.toml`); zstd and its crates moved to versions
  without the open advisories.

## Since the 0.1.0 releases

`strata-js-v0.1.0` (2 November 2026), `strata-py-v0.1.0` (5 November) and
`strata-server-v0.1.0` (6 November) were released under the old name. In
the weeks since, in broad strokes:

- Strata became ChloeDB and Engram became ChloeCtx: crates, programs,
  packages (`import chloedb`, `chloedb` on npm, `ai.chloetech.chloedb`),
  settings (`CHLOEDB_*`), headers and metrics carry the new names, and the
  old ones are still accepted for a release (see
  [AGENTS.md](AGENTS.md) for what keeps the old name for now).
- Clusters: following another cluster (a copy feed and catching up),
  switching over to a follower, stream offsets that move with their slots,
  failover elections, and scrolls through a coordinator; the shard protocol
  is now 18.
- Storage and writes: many writers at once sharing commits, embedded
  databases that merge as they write, a compact primary-key index, and a
  checksum per doc-store block.
- Queries: GROUP BY and aggregates read from columns, MEDIAN and
  percentiles, date functions, vector partitions for filtered vector
  search.
- Operations: the command center (alerts, on-call, the operator's view),
  the chaos tests, the app engine as a cluster app, and prebuilt Python
  wheels and JavaScript tarballs per platform.
