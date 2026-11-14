# ChloeDB from JavaScript

The `chloedb` package opens a ChloeDB file inside a Node, Bun or Deno process, the way better-sqlite3 opens a SQLite file: no server, no configuration. It can also talk to a running [chloedb-server](server.md) from any JavaScript runtime with `fetch` (a browser, a worker, Node, Bun, Deno). Both have the same methods, so code written against a local file works against a server.

## Install

The package is in [`chloedb-js/`](https://github.com/chloetech-ai/chloedb/tree/main/chloedb-js): a Node-API addon written in Rust ([napi-rs](https://napi.rs)) and the JavaScript around it, an ES module, for Node 18 or later, Bun and Deno.

### Prebuilt

Each `chloedb-js-v<version>` release on [GitHub](https://github.com/chloetech-ai/chloedb/releases) has a tarball per platform with the addon built in, which installs with no Rust and no compiler. Install the tarball for your platform:

```sh
npm install https://github.com/chloetech-ai/chloedb/releases/download/chloedb-js-v0.3.3/chloedb-0.3.3-darwin-arm64.tgz
```

It imports as `"chloedb"`. Windows and Alpine (musl) have no tarball: build from source there (Windows through WSL 2). Release 0.1.0, from before the rename, was published as `strata-db` and imports as `"strata-db"`.

| platform | tarball |
|---|---|
| macOS 11+ on Apple silicon | `chloedb-<version>-darwin-arm64.tgz` |
| macOS 11+ on Intel | `chloedb-<version>-darwin-x64.tgz` |
| Linux x86-64, glibc 2.35+ (Ubuntu 22.04, Debian 12 and later) | `chloedb-<version>-linux-x64-gnu.tgz` |
| Linux arm64, glibc 2.35+ | `chloedb-<version>-linux-arm64-gnu.tgz` |

The workflow [`js-release.yml`](https://github.com/chloetech-ai/chloedb/blob/main/.github/workflows/js-release.yml) builds the tarballs on a `chloedb-js-v<version>` tag and runs each one in an empty app before attaching it to the release; `node scripts/pack.mjs <platform>` packs one by hand.

`chloedb` on npm is a placeholder (0.0.x) that holds the name until the releases are published there: a bare `npm install chloedb` installs it, and importing it throws an error that says how to install ChloeDB. The same workflow builds what npm will get, the way napi-rs packages are laid out: `chloedb`, the JavaScript, with a package per platform holding its addon (`@chloedb/linux-x64-gnu`, `@chloedb/darwin-arm64`, ...) as optional dependencies, of which npm installs the one of your machine. A maintainer publishes them with a run started by hand on the release's tag (`gh workflow run js-release.yml --ref chloedb-js-v<version> -f npm=true`; the workflow's header says what it needs first). Once a release is there, `npm install chloedb` is all it takes, and this page will say so. `node scripts/pack.mjs --npm <platform>` and `--npm-main` pack those packages by hand.

### From source

It needs Rust (stable) and a C compiler.

```sh
cd chloedb-js
npm run build                    # cargo build --release, then copies the addon to chloedb.node
cd /path/to/my-app
npm install /path/to/chloedb/chloedb-js  # or "chloedb": "file:/path/to/chloedb/chloedb-js" in package.json
```

`npm install` of the directory builds the addon itself when `chloedb.node` is missing. `npm run build -- --features bert` builds more of ChloeDB's features in (`bert`, `onnx`, `vision`, `audio`; `datafusion`, `parquet` and `s3` are on by default). `$CHLOEDB_NODE_ADDON` (or `$STRATA_NODE_ADDON`) loads an addon from another place.

The client alone (`chloedb/client`) is plain JavaScript with no addon: a bundler takes it into a page, and the package's `browser` and `workerd` conditions give it for `chloedb` too.

## A database in the process

```js
import { open } from "chloedb";

const db = await open("books.chloedb", {
  schema: {
    table: "books",
    key: "id",
    fields: [
      { name: "id", type: "keyword" },
      { name: "title", type: "text" },
      { name: "year", type: "int" },
      { name: "emb", type: "vector", embed: { from: ["title"], model: "potion-retrieval-32M" } },
    ],
  },
});

await db.upsert([{ id: "b1", title: "The river and the dragon", year: 1999 }]);
for (const row of await db.search("dragon")) console.log(row.id, row.title);
const r = await db.query("SELECT year, COUNT(*) FROM books GROUP BY year");
r.rows;          // [[1999, 1]]
await db.close();
```

`open(path, {schema})` opens the file, or creates it with the schema when it does not exist; `Database.create(path, schema)` and `Database.open(path)` do one or the other. A schema is an object, JSON text or the path of a JSON file, in the [short form or the long one](server.md#schemas). `directory: true` keeps a database as a directory of segment files, `models` is where embedded fields' models are (`$CHLOEDB_MODELS`, else `~/.local/share/chloedb/models` (or `~/.local/share/strata/models`, on a machine that has that folder from before the rename; before 0.3.2, always); `chloedb-server fetch-model` fetches them), and `threads` how many cores index builds and each query use (every core by default).

Every call answers a promise and runs on a worker thread of Node's pool, so queries and writes leave the event loop free and many run at once. Writes made at once wait for each other and share commits (one disk flush for all those waiting), so none fails as busy. One process at a time may have a database open to write it (a second one gets "busy: the database is open elsewhere"). Others can open it with `Database.open(path, { readOnly: true })` to read it while it is written, the workers of a cluster, say: each query sees the writer's commits so far, every write fails, and `refresh()` looks for new commits at once ([how readers follow the writer](index.md#readers-in-other-processes)). For writes from several processes, serve it with chloedb-server.

### Querying

| call | what |
|---|---|
| `db.query(sql, params, {timeoutMs, memory, signal})` | a SELECT: a `Result` ([limits](#limits-memory-deadlines-aborting)) |
| `db.sql(sql, params, {timeoutMs, memory, signal})` | any statement: a SELECT's `Result`, or the rows an INSERT, UPDATE or DELETE wrote; `CREATE TABLE`, `ALTER TABLE` and `DROP TABLE` too |
| `db.search(text, {limit, where, columns, mode, textFields, vectorField, params, table})` | the rows that best match a text: BM25 over the text fields (`mode: "text"`), the similarity of an embedded vector field (`"vector"`), or both fused by reciprocal rank (`"hybrid"`, what `"auto"` picks when a model is open) |
| `db.plan(sql, params)`, `db.run(plan)` | a statement's [plan](query-language.md) as JSON, without running it; running a plan |
| `db.embed(text, {field, image, audio})`, `db.embedTokens(text, field)` | a query vector by an embedded field's model (of an image's or a sound's bytes too), a multi-vector field's token vectors |
| `db.queryLogged(sql, params, {context, variant, ...})`, `db.exportRankLog({...})` | the [rank log](query-language.md#the-rank-log-what-each-query-served): a ranked query that logs what it served, and the log as records for training |
| `db.sidNext(prefixes, {field, where})`, `db.trainCodebook(field, name, {balanced})` | semantic IDs: the codes that can follow prefixes, and training a codebook (`balanced: true` uses every level's numbers evenly) |

Parameters are `:name` in the SQL and an object of values: numbers, strings, booleans, null, `Date`s, arrays (an array of numbers or a `Float32Array` is a vector), an object of numbers (sparse term weights), `{embed: "some text"}` (the text's vector by an embedded field's model) or `{vector_of: key}` (a stored row's vector).

A `Result` has `columns`, `rows` (arrays in column order), `keys` (each row's primary key), `scores` (what it was ranked by) and `stats` (what the engine did). Iterating it gives each row as an object; `r.at(i)`, `r.objects()`, `r.column(name)` and `r.scalar()` (a count, a sum) read it other ways.

### Limits: memory, deadlines, aborting

A query runs in your process, so what it builds up (its groups, the rows it orders and returns, a join's tables) is held by your program. The queries of every database a process opens share a memory budget, by default a quarter of the machine's memory (the cgroup's limit in a container, else RAM), or `CHLOEDB_QUERY_MEMORY_MB` MiB when that is set when the first database opens (`0`: no budget). A query that would hold more rejects with a `ChloeDBError` of code `memory`, naming the budget and how to raise it, and gives back what it held, instead of the process running out of memory.

A query has no deadline unless it is given one. `db.query`, `db.sql`, `db.run` and `db.search` take a last argument of options: `timeoutMs` (its deadline, as a `Client`'s `query` takes it: it rejects with `DeadlineError` naming it), `memory` (a budget of its own in bytes; `0`: none) and `signal`, an `AbortSignal` that stops it when it aborts (it rejects with the signal's reason, as an aborted `fetch` does). A database's own limits, for every query that does not give its own, are set when it opens or later; `db.interrupt()` stops every query of the database running now, and returns how many there were:

```js
const db = await open("books.chloedb", { timeoutMs: 5000, memory: 512 * 2 ** 20 });  // 5 s, and 512 MiB its queries share
await db.query("SELECT genres, COUNT(*) FROM books GROUP BY genres", null, { timeoutMs: 200 });
await db.query("SELECT * FROM books", null, { memory: 0, signal: AbortSignal.timeout(1000) });
db.setLimits({});                 // back to the defaults: no deadline, the process's budget
db.timeoutMs, db.memory;          // the limits now (null: none)
```

A query checks its deadline and its signal between segments and every 1,024 rows it groups, orders, walks or joins, so it stops within milliseconds; it holds a thread of libuv's pool (`UV_THREADPOOL_SIZE`, 4 by default, which `fs` and `dns` share) until then. A write is not cut short: `sql`'s limits apply to a SELECT.

### Writing

| call | what |
|---|---|
| `db.insert(rows, {table})` | rows whose keys are new; all or none are written. 10,000 rows or more are bulk-loaded into new segments in one commit |
| `db.upsert(rows, {table})` | rows, replacing the rows with their keys, in transactions of 10,000 |
| `db.delete(keys, {table})` | the rows with these keys |
| `db.write(ops)` | [write operations](server.md#http-api) in one transaction: insert, upsert, delete, `set` of a field (a number in place, another by writing the row again) and `add` to a number in place |
| `db.get(key)`, `db.getMany(keys)` | rows by key without SQL, or null |
| `db.addColumn(name, type, options)`, `dropColumn`, `renameColumn` | `ALTER TABLE` |
| `db.expire()`, `db.optimize()`, `db.vacuum()`, `db.close()` | rows past their `expire_by`; a table that is done changing readied for queries (its segments merged into as few as can be, deleted rows dropped, the file shrunk); the space merges freed returned to the file system; the database closed, after merging small segments and returning the space merges freed when it is an eighth of the file or more (`await using db = ...` closes it too). Segments merge in the background as writes add them, one merge at a time |

A row is an object of field name to value. A vector may be a `Float32Array` (read from its bytes, without a number per element in between), a `Float64Array` or an array; a TIMESTAMP a `Date` or ISO 8601 text; a list field an array or a `Set`. Values come back as chloedb-server answers them: TIMESTAMPs as ISO 8601 text in UTC, vectors as arrays of numbers.

An INT past 2^53 (a 64-bit key, a hash) comes back as a `BigInt`, and a `BigInt` may be written or passed as a parameter anywhere a number may; every other integer is a number. (Reading them whole takes a runtime whose `JSON.parse` gives a reviver the number's text: Node 21 and later, Deno, recent browsers.)

### Files

| call | what |
|---|---|
| `db.importFile(path, {format, table})` | a JSON Lines, JSON, CSV or TSV file: bulk-loaded into an empty table, else upserted; returns `{rows, bulk, skippedFields}` |
| `fromSqlite(sqlitePath, path)`, `planSqlite`, `db.importSqlite`, `db.exportSqlite` | [SQLite](sqlite.md) in and out |
| `fromParquet(parquetPath, path)`, `planParquet`, `db.importParquet`, `db.exportParquet` | [Parquet](parquet.md) files and directories in and out, and `s3://` URLs |
| `upgrade(path)` | a database from before field ids, copied into this version's format |

Errors reject with a `ChloeDBError`, whose message is the database's (`StrataError`, its name before the rename, is the same class), and whose `code` is stable, the same here, from a server and in Python: the server's JSON `code` when it gives one, else the kind the message starts with (`busy: ...`), else the HTTP status. The common codes reject as subclasses of `ChloeDBError`, so `instanceof ChloeDBError` still holds for every one:

| code | class | when |
|---|---|---|
| `bad_request` | `BadRequestError` | a bad schema, query, value or request (400, 413, 422) |
| `unauthorized` | `UnauthorizedError` | no token, or one without the rights needed (401, 403) |
| `not_found` | `NotFoundError` | no such route, table or cursor (404) |
| `conflict` | `ConflictError` | refused by the state it met (409): an `ifSeqNo` that no longer holds, an idempotency key still running, a `minSeqNo` past the server's last write |
| `partial_write` | `PartialWriteError` | a coordinator wrote some shards and not others: send it again with the same idempotency key |
| `busy` | `BusyError` | the database is open in another process, or another writer holds it |
| `overloaded` | `OverloadedError` | refused for the server's load (429), after the client's retries |
| `timeout` | `DeadlineError` | a deadline passed: `timeoutMs`, a coordinator's wait for a shard (504) |
| `unavailable` | `UnavailableError` | no copy can serve it now (503) |

Other codes are `memory` (a query past its memory budget), `corrupt`, `io`, `internal` and `error`; `errorCode(message, status, answer)` gives the code of any message.

## A server from JavaScript

```js
import { connect } from "chloedb/client";   // or from "chloedb"

const client = connect("http://127.0.0.1:7700", { token: process.env.CHLOEDB_TOKEN });
await client.upsert([{ id: "b2", title: "A second book", year: 2001 }]);
const r = await client.search("book", { limit: 5 });
```

`Client` has the methods of `Database` that a server offers, over its [HTTP API](server.md#http-api), and more of the server's own: `health()`, `stats()`, `explain(sql)`, `put(key, row, {ifSeqNo})`, `stream(events)` (an iterable or async iterable of [stream events](server.md#streams), applied once), `source(name)` and `sources()`, `ask(question)` (the server's agent), and the file calls, which upload a path's bytes (Node, Bun, Deno) or bytes given (a `Uint8Array`, a `Blob`). `exportSqlite(null)` and `exportParquet(null)` give the file's bytes instead of writing it. Rows are written 1,000 a request, keys read 10,000 a request. An error the server answers rejects with a `ChloeDBError` whose `status` is the HTTP status and `answer` its JSON; one where the server cannot be reached rejects with `fetch`'s error. `timeout` (milliseconds, 30 seconds by default) bounds each request, and `fetch` takes another fetch. A request the server refuses for its load (429: [behind on flushing](server.md#streams) its writes, its write queue full, too many queries running) did nothing, so the client sends it again after the `Retry-After` the server gives, for up to `retryFor` milliseconds (10 seconds; 0 sends each request once), and rejects with the 429 after that. A write that failed partway on a cluster (an error with `"partial": true`) is not sent again, since the shards that took it keep it, unless it has an idempotency key.

`write(ops, {idempotencyKey})` and `sql(sql, params, {idempotencyKey})` send a write with an [idempotency key](server.md#when-a-write-fails), so that sending it again does not apply it twice. A write that failed partway through a coordinator rejects with a `ChloeDBError` whose `answer` says so (`partial: true`) and names its key: `await client.write(ops, { idempotencyKey: e.answer.idempotency_key })` writes the rest.

### Reading your own writes

A read from a [replica](server.md#replication-a-primary-and-its-replicas), or through a coordinator whose shards have replicas, can be answered by a copy that has not applied a write yet. Servers that number their writes (`--seq-no`) fix that with sequence numbers ([reads that see a write](server.md#reads-that-see-a-write)):

- `writeResult(ops, {idempotencyKey})` is `write` answering `{written, seqNo, seqNos, skipped}`: `seqNo` from a server, `seqNos` (one per shard) from a coordinator, neither from a server without `--seq-no`. `write`, `insert`, `upsert`, `delete`, `put` and `sql` still answer the number of rows.
- `query(sql, params, {minSeqNo, minSeqNos})`, `search(text, {minSeqNo, minSeqNos})`, `get(key, {minSeqNo, minSeqNos})`, `getMany`, `sql` (a SELECT), `run` (a read plan) and `sidNext` take them: the copy that answers holds those writes (a replica waits up to a second; a server refuses a number past its last write, 409).
- `client.session()` (or `new Session(url, options)`) is a client that does this itself: it keeps the latest numbers its own writes answered (`session.seqNo`, `session.seqNos`) and sends them with each of its reads, so reading your own writes is one line. `session.observe(answer)` adds another client's write.

```js
const s = connect("http://coordinator:7700").session();
await s.upsert({ id: "u1", name: "Ada" });
await s.get("u1"); // sees the upsert, whichever copy answers

const w = await client.writeResult([{ op: "upsert", doc: { id: "u2", name: "Grace" } }]);
await client.get("u2", { minSeqNo: w.seqNo, minSeqNos: w.seqNos });
```

The numbers go with reads only: a server refuses them on a write, so a session leaves them off an `INSERT`, `UPDATE` or `DELETE` by `sql`, judging a statement by its first word. A `Database` in the process ignores them: it reads every write it committed.

In a browser, the client works from pages of the server's own origin (a page it serves, or behind the same reverse proxy): chloedb-server sends no CORS headers, and refuses writes from pages of other sites ([security](server.md#security)).

## Search on the device

`openNearby(url, table, dir)` holds a [slice](slices.md) of a chloedb-server's table near a user: the rows of the tiles around them in a database in `dir`, searched with the server's statistics. `moveTo(lat, lon)` fetches the tiles it lacks, and `search(q, { k, d0 })` answers on the device when it can prove the answer is the server's, and asks the server otherwise; the result's `where` is `"device"` or `"server"` and `why` the reason. For a [vector slice](slices.md#vector-slices-more-like-this-on-the-device), `focus(taste, { rows })` holds the clusters nearest a taste vector and `similar(vector, { k })` answers a similarity search the same way, fetching the few clusters its codes name when that proves the answer.

```js
import { openNearby } from "chloedb";

const near = await openNearby("http://127.0.0.1:7700", "places", "./slice");
await near.moveTo(40.7484, -73.9857);
const r = await near.search("pizza", { k: 10 });
console.log(r.where, r.why);   // device proved
```

## Bun and Deno

Bun loads the addon as Node does: `bun run app.js`. Deno loads it through its Node compatibility with `--allow-ffi` (and `--allow-read`, `--allow-write`, `--allow-env` for the files and `$CHLOEDB_MODELS`); the client needs only `--allow-net`.

## Tests

```sh
cd chloedb-js
npm run build -- --profile ci             # optimised, built faster than release
npm test                                  # node --test
```

The client's tests start `chloedb-server/target/release/chloedb-server` (or `$CHLOEDB_SERVER`) and are skipped when it is not built.
