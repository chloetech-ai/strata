# ChloeDB from Java

The `ai.chloetech.chloedb` package runs a ChloeDB database inside a Java process, the way sqlite-jdbc runs SQLite: the database is a file the JVM opens, read and written by ChloeDB's native library over JNI, with no server and nothing to configure. It has a JDBC driver too (`jdbc:chloedb:books.chloedb`), for code and tools that speak JDBC. It works on Java 11 and later.

Before ChloeDB was renamed, the package was `ai.chloetech.strata` and its jar `ai.chloetech:strata`: change the imports and the dependency when you rebuild (`StrataException` is `ChloeDBException`, and the driver's `Strata*` classes are `ChloeDB*`). URLs that begin `jdbc:strata:` still open a database.

## Build it

The package is in [`chloedb-java/`](https://github.com/chloetech-ai/chloedb/tree/main/chloedb-java): Java classes (`src/main/java`) and the Rust crate of their native library (`src/*.rs`). Building it needs Rust (stable), a C compiler, a JDK (11 or later) and Maven.

```sh
cd chloedb-java
mvn package          # cargo builds the library, Maven the classes; tests run against both
mvn install          # and into ~/.m2, as ai.chloetech:chloedb:0.3.3
```

The jar (`target/java/chloedb-0.3.3.jar`) carries the native library for the platform it was built on (Linux or macOS, x86-64 or ARM), and needs nothing else at run time: no dependency, no server. A project uses it from the local repository:

```xml
<dependency>
  <groupId>ai.chloetech</groupId>
  <artifactId>chloedb</artifactId>
  <version>0.3.3</version>
</dependency>
```

or, with Gradle, `implementation("ai.chloetech:chloedb:0.3.3")` with `mavenLocal()` among the repositories.

`-Dcargo.profile=ci` builds the library without link-time optimisation, much faster, for a build that only runs the tests. `-Dcargo.features=bert` (comma-separated) turns on Cargo features as the Python package has them: `bert` for BERT embedding models (`accelerate` on a Mac), `onnx`, `vision`, `audio`. The library already includes DataFusion, Parquet and `s3://` URLs. `-Dcargo.skip=true` packages the library cargo built before.

## A database in your process

```java
import ai.chloetech.chloedb.Database;
import ai.chloetech.chloedb.Result;
import java.util.List;
import java.util.Map;

Map<String, Object> schema = Map.of("table", "books", "key", "id", "fields", List.of(
    Map.of("name", "id", "type", "keyword"),
    Map.of("name", "title", "type", "text"),
    Map.of("name", "year", "type", "int"),
    Map.of("name", "emb", "type", "vector", "embed", Map.of("from", "title", "model", "hash:32"))));

try (Database db = Database.create("books.chloedb", schema)) {
    db.insert(List.of(
        Map.of("id", "b1", "title", "The river and the dragon", "year", 1999),
        Map.of("id", "b2", "title", "A mountain in winter", "year", 2004)));
    for (Map<String, Object> row : db.search("dragon")) {
        System.out.println(row.get("id") + " " + row.get("title"));
    }
    Result r = db.query("SELECT year, COUNT(*) AS n FROM books WHERE year >= :y GROUP BY year", Map.of("y", 2000));
    r.rows();                                  // [[2004, 1]]
    db.execute("UPDATE books SET year = 2000 WHERE id = :k", Map.of("k", "b1"));   // 1
}

try (Database db = Database.open("books.chloedb")) {
    db.get("b1");                              // {id=b1, title=The river and the dragon, year=2000}
}
```

`Database.create` takes the schema as a map (the [short form](server.md#schemas) or the long one), JSON text, or the path of a JSON file. `Database.open` opens a database the file already holds. Both take `new Database.Options()`: `.directory(true)` keeps a new database as a directory of segment files, `.models(dir)` says where embedding models are, `.threads(n)` how many cores queries and index builds use (the machine's by default).

Only one process may have a database open to write it at a time; another gets a `ChloeDBException` saying the database is busy. Other processes can open it with `Database.open(path, new Database.Options().readOnly(true))` to read it while it is written: each query sees the writer's commits so far, every write fails, and `refresh()` looks for new commits at once ([how readers follow the writer](index.md#readers-in-other-processes)). For writes from several processes, serve it with [chloedb-server](server.md). Within the process, one `Database` is safe to use from many threads at once: queries run in parallel, writes made at once wait for each other and share commits (one disk flush for all those waiting, none failing as busy), and a call into the library does not hold the JVM. Segments merge in the background as writes add them, one merge at a time. Close the database when done, with `close()` or try-with-resources, which also merges small segments and gives the file system back the space merges freed when it is an eighth of the file or more; one left open is closed when the garbage collector finds it.

## Rows and values

A row is a map of field name to value, and a query's answer gives values back as Java types:

| ChloeDB | written as | read as |
|---|---|---|
| KEYWORD, TEXT | `String` | `String` |
| INT | `Integer`, `Long` (any integer type, `BigInteger` within 64 bits) | `Long` |
| FLOAT | `Double`, `Float`, `BigDecimal` | `Double` |
| BOOL | `Boolean` | `Boolean` |
| TIMESTAMP | `Instant`, `OffsetDateTime`, `ZonedDateTime`, `Date`; `LocalDateTime` and `LocalDate` as UTC; ISO 8601 text | `Instant` |
| VECTOR | `float[]` (kept as float32), `double[]`, a `List` of numbers | `float[]` |
| a multi-valued field | a `List` | a `List` |
| SPARSE | a map of term to weight | a `String` of JSON |
| JSON | a `Map`, or JSON text | its JSON text |

`null` is NULL. Maps keep their order where it matters (`LinkedHashMap`); `Map.of` takes no null, so a row with a NULL field is a `HashMap` or a `LinkedHashMap`. A value of another type throws `IllegalArgumentException` before anything is written.

`insert` writes rows whose keys must be new, all or none: 10,000 rows or more are bulk-loaded into new segments in one commit, fewer go through the write-ahead log. `upsert` inserts or replaces rows by key, `delete` takes a key or a collection of them, and `get` and `getMany` read rows by key without SQL (`null` for a key no row has). `write` takes the operations chloedb-server's `/v1/write` takes (`insert`, `upsert`, `delete`, `set` of a field, a number in place and another by writing the row again, and `add` to a number in place) and applies them in one transaction.

## Queries

`query` runs a SELECT (or an EXPLAIN) and gives a `Result`; `execute` runs any other statement (INSERT, UPDATE, DELETE, CREATE TABLE, ALTER TABLE) and gives the rows it wrote. Parameters (`:name` in the SQL) are a map:

```java
float[] v = db.embed("dragon river");      // the text's vector by an embedded field's model
db.query("SELECT id FROM books ORDER BY similarity(emb, :v) DESC LIMIT 5", Map.of("v", v));
db.query("SELECT id FROM books ORDER BY similarity(emb, :v) DESC LIMIT 5", Map.of("v", Map.of("embed", "dragon river")));
db.query("SELECT id FROM books ORDER BY similarity(emb, :v) DESC LIMIT 5", Map.of("v", Map.of("vector_of", "b1")));
```

A `Result` has `columns()`, `rows()` (lists in column order), `keys()` (each row's primary key), `scores()` (what each row was ranked by) and `stats()` (what the engine did, as JSON text). Iterating it gives each row as a map of column to value; `row(i)`, `column(name)` and `scalar()` (the first value of the first row: a count) read parts of it. `plan(sql)` gives a statement's plan as a map without running it.

`search(text)` gives the rows that best match a text, as the Python package's `search` does: BM25 over the text fields, the similarity of an embedded vector field to the text's embedding, or both fused by reciprocal rank, which it picks when the table has an embedded field whose model is open.

```java
db.search("dragon", new SearchOptions().limit(5).where("year >= :y").param("y", 1950).columns("id", "year"));
db.search("dragon", new SearchOptions().mode("text").textFields("title"));
```

`queryLogged` and `exportRankLog` keep and export a [rank log](query-language.md#the-rank-log-what-each-query-served) for training a ranking, with the options chloedb-server takes for them.

### Limits: memory, deadlines, interrupting

A query runs in your process, so what it builds up (its groups, the rows it orders and returns, a join's tables) is held by the JVM's process, outside its heap. The queries of every database a process opens share a memory budget, by default a quarter of the machine's memory (the cgroup's limit in a container, else RAM), or `CHLOEDB_QUERY_MEMORY_MB` MiB when that is set when the first database opens (`0`: no budget). A query that would hold more throws a `ChloeDBException` whose message starts `memory:`, naming the budget and how to raise it, instead of the process running out of memory.

A query has no deadline unless it is given one. `query(sql, params, options)`, `execute(sql, params, options)` and `SearchOptions.limits(options)` take a `QueryOptions`: `timeoutMs` (past it the query stops and throws a `ChloeDBException` whose message starts `timeout:`, naming it) and `memory` (a budget of its own in bytes; `0`: none). A database's own limits, for every query that does not give its own, come with `Database.Options` or `setLimits`; `interrupt()`, from any thread, stops the queries of the database running now and returns how many there were:

```java
Database db = Database.open("books.chloedb", new Database.Options().timeoutMs(5_000).memory(512L << 20));
db.query("SELECT genres, COUNT(*) FROM books GROUP BY genres", null, new QueryOptions().timeoutMs(200));
db.query("SELECT * FROM books", null, new QueryOptions().memory(0));   // counts nothing
db.setLimits(null, null);                                              // back to the defaults
```

A query checks its deadline and `interrupt` between segments and every 1,024 rows it groups, orders, walks or joins, so it stops within milliseconds. A write is not cut short. Through JDBC, `Statement.setQueryTimeout(seconds)` is each query's deadline (past it, a `SQLTimeoutException`, state `HYT00`) and `Statement.cancel()`, from another thread, stops the statement's query (a `SQLException`, state `HY008`).

## Tables

The methods that act on a table (`insert`, `upsert`, `delete`, `get`, `getMany`, `search`, `schema`, `importFile`, `addColumn`, `dropColumn`, `renameColumn`, `embed`) act on the database's first table. `db.table("events")` gives another, with the same methods:

```java
db.execute("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD, book KEYWORD REFERENCES books, ts TIMESTAMP)");
Table events = db.table("events");
events.insert(Map.of("id", 1, "usr", "u1", "book", "b1", "ts", Instant.now()));
db.query("SELECT b.title FROM events e JOIN books b ON e.book = b.id WHERE e.usr = 'u1'");
```

`tables()` gives every table's schema. `addColumn("price", "float", Map.of("default", 1.5))`, `dropColumn` and `renameColumn` change a table's columns, as `ALTER TABLE` does.

## Files

```java
db.importFile("rows.jsonl");                           // JSON Lines, JSON, CSV or TSV, by its name
db.importSqlite("notes.sqlite");                       // a SQLite file's tables
db.exportSqlite("out.sqlite", Map.of("fts", false));   // and the database out to one
db.importParquet("s3://bucket/events/");               // Parquet files, from S3 too
db.exportParquet("out/");
Database copy = Database.fromSqlite("notes.sqlite", "notes.chloedb");  // a new database from a SQLite file
```

Their options are maps, with the names the [SQLite](sqlite.md) and [Parquet](parquet.md) pages give (`tables`, `schemas`, `blobsBase64`, `fts`, `indexes`, `table`, `schema`, `key`). An option a method does not take throws `IllegalArgumentException` naming those it takes, before anything is read or written. `Database.planSqlite` and `Database.planParquet` say what an import would do without doing it, and `lastImport()` what the import that made a database did.

## JDBC

The jar is also a JDBC driver: `jdbc:chloedb:PATH` (or `jdbc:strata:PATH`, by the old name) opens the database at `PATH` in the process, as `jdbc:sqlite:PATH` does a SQLite file. `DriverManager` finds it by itself (the jar lists it under `META-INF/services`).

```java
import java.sql.*;

try (Connection c = DriverManager.getConnection("jdbc:chloedb:books.chloedb");
     PreparedStatement ps = c.prepareStatement("SELECT id, title, year FROM books WHERE year >= ? ORDER BY year LIMIT ?")) {
    ps.setInt(1, 2000);
    ps.setInt(2, 10);
    try (ResultSet rs = ps.executeQuery()) {
        while (rs.next()) {
            System.out.println(rs.getString("id") + " " + rs.getString("title") + " " + rs.getInt("year"));
        }
    }
}
```

Options follow a `?` in the URL or come as connection properties: `schema` (the path of a schema's JSON file, with which a database that does not exist yet is created: `jdbc:chloedb:books.chloedb?schema=books.json`), `models`, `threads`, and `readonly=true`, which refuses statements that write. A URL may also name the path as `jdbc:chloedb:file:books.chloedb`.

- **Statements** are ChloeDB's SQL ([the query language](query-language.md)), with `?` parameters numbered from 1. `setObject` takes the values the rest of the package takes: a `float[]` is a vector (`ORDER BY similarity(v, ?)`), a `List` a list. Batches (`addBatch`, `executeBatch`) run one statement after another; one that fails stops the batch with a `BatchUpdateException` that counts the ones before it, which stay written.
- **Results** come back whole, so a result set can scroll if the statement is made with `ResultSet.TYPE_SCROLL_INSENSITIVE`; result sets are read-only. `getObject` gives an INT as `Long`, a FLOAT as `Double`, a TIMESTAMP as `java.sql.Timestamp`, a vector as `float[]` and a list as `List`; the getters convert (`getInt`, `getString`, `getObject(i, Instant.class)`, `getArray` for vectors). Times are in UTC.
- **Transactions**: each statement commits as it runs, as it does through the rest of the package. Auto-commit is always on: `setAutoCommit(false)` throws `SQLFeatureNotSupportedException` rather than pretending a rollback could undo anything, and `commit()` and `rollback()` throw too. A tool or framework that needs transactions across statements cannot use ChloeDB through JDBC yet.
- **Connections share the database.** A process may have a database open once, so the connections to one file (a pool's, say) share one `Database`, which the last of them to close closes. `connection.unwrap(Database.class)` gives it, for what SQL does not do: `search`, `importParquet`, `embed`.
- **Metadata** (`getMetaData()`) lists tables, columns with their types (KEYWORD and TEXT as `VARCHAR`, INT as `BIGINT`, FLOAT as `DOUBLE`, BOOL, TIMESTAMP, vectors and lists as `ARRAY`), keys, references and the SQL types: what database browsers ask a driver. Tables have no catalog or schema, and table and column patterns match regardless of case, as ChloeDB's SQL names do.
- **Errors** are `SQLException`s: a statement ChloeDB cannot parse or plan a `SQLSyntaxErrorException`, a query past `setQueryTimeout` a `SQLTimeoutException` and one stopped by `cancel()` state `HY008` ([limits](#limits-memory-deadlines-interrupting)), what the driver does not do a `SQLFeatureNotSupportedException`.

## Errors

The database's errors (a bad schema, query, value or file, a database open elsewhere) are `ChloeDBException`s, with the message the HTTP API and the Python package give. An argument of the wrong kind is an `IllegalArgumentException`, and a call on a closed database an `IllegalStateException`.

## How it works

The JNI boundary is two functions: `call(handle, method, args)` runs one of the library's methods with its arguments as one value and gives its answer as another, both in a small tagged binary form ([`codec.rs`](https://github.com/chloetech-ai/chloedb/blob/main/chloedb-java/src/codec.rs), and `Codec.java` on the Java side), and `free(handle)` drops a database once Java no longer holds it. So no JNI call is made per value, and what a method does lives in one place ([`calls.rs`](https://github.com/chloetech-ai/chloedb/blob/main/chloedb-java/src/calls.rs)), as the Python package does it: rows, schemas and options are read by the code that reads files and the HTTP API. JNI rather than the Foreign Function and Memory API (Panama) because Panama is final only from Java 22, and JNI runs on every Java from 11.

The library is loaded from the path in the system property `chloedb.library` (or `CHLOEDB_JNI_LIBRARY`; the old names `strata.library` and `STRATA_JNI_LIBRARY` work too) if set, else from the jar, unpacked to a temporary file that is removed once loaded, else from `java.library.path`. A jar and a library built from different checkouts refuse each other. Java 22 and later warn when a library is loaded without `--enable-native-access=ALL-UNNAMED` (the jar's manifest says so for an executable jar); pass it to the JVM.

The Python package's query builder (`chloedb.Query`), Arrow answers, client of chloedb-server and training helpers have no Java counterpart yet: queries are SQL.
