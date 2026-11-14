# ChloeDB's query language

ChloeDB speaks standard SQL over the tables of a database: `SELECT` with functions for full text, vectors, rank fusion and graphs, aggregates, typo-tolerant and synonym searches, and subqueries, `WITH` and joins of a table with itself; `INSERT`, `UPDATE` and `DELETE`; `CREATE TABLE`, `ALTER TABLE` and `DROP TABLE`; and functions stored in the database (`CREATE FUNCTION`), which queries call. Text, structured and vector conditions mix freely in one statement, and the planner decides how to evaluate them together.

## A first look

```sql
-- boolean full text and structured filters, ranked by BM25
SELECT id, title, score() FROM papers
WHERE MATCH(body, 'vaccine AND (mrna OR "messenger rna") NOT animal')
  AND year BETWEEN 2019 AND 2021 AND category IN ('virology', 'immunology')
ORDER BY bm25(body, 'mrna vaccine efficacy') DESC LIMIT 10;

-- filtered vector search
SELECT id FROM papers WHERE rating >= 4
ORDER BY similarity(emb, :q) DESC LIMIT 10;

-- hybrid: a weighted sum of normalised scores, or reciprocal rank fusion
SELECT id FROM papers WHERE year >= 2015
ORDER BY 0.2 * tmm(bm25(body, :text)) + 0.8 * tmm(similarity(emb, :q)) DESC LIMIT 10;

SELECT id FROM papers WHERE year >= 2015
ORDER BY rrf(bm25(body, :text), similarity(emb, :q)) DESC LIMIT 10;

-- any expression of scores and columns
SELECT id FROM papers
ORDER BY ln(1 + bm25(body, :text)) * (1 + 0.2 * coalesce(rating, 0)) DESC LIMIT 10;

-- facets and aggregates over any result
SELECT category, COUNT(*), AVG(rating), MAX(year) FROM papers WHERE MATCH(body, 'vaccine')
GROUP BY category HAVING COUNT(*) >= 10 ORDER BY 2 DESC LIMIT 5;

-- typos and synonyms
SELECT id FROM papers WHERE MATCH(body, 'vacine~ OR mrna', synonyms => 1) LIMIT 10;

-- a join along citations, ranked by one side
SELECT a.id, a.title, b.id AS cited, b.title AS cited_title
FROM papers a JOIN papers b ON a.cites = b.id
WHERE a.year >= 2020 ORDER BY bm25(a.body, 'mrna vaccine') DESC LIMIT 20;
```

## Statement shape

```text
[WITH name AS (SELECT ...), ...]
SELECT [DISTINCT | DISTINCT ON (<expressions>)] <columns | * | score() | hops() | seq_no() | features() | ranking expressions | aggregates | window functions>
FROM <table> [JOIN <table> ON ...] | <WITH name> | (SELECT ...) AS t
[WHERE <predicate>]
[GROUP BY <columns> [HAVING <predicate over aggregates>]]
[QUALIFY <predicate over window functions>]
[WINDOW name AS (<window>), ...]
[{UNION | INTERSECT | EXCEPT} [ALL] SELECT ...]
[ORDER BY <score, column or aggregate> [ASC | DESC] [NULLS FIRST | LAST] [, <more keys> [ASC | DESC]] [MAX k PER <field>]]
[PACK n TOKENS [OF <columns>] [EXCERPT n] [DEDUPE x [ON <vector field>]] [FOLLOW <edges field> [n HOP]] [WITH RECEIPT]]
[LIMIT n [OFFSET m]]
```

- **Tables by name.** A database holds one or more [tables](#tables), and a query reads the tables it names (in any case): one, or several joined, read by subqueries or named in `WITH` ([below](#subqueries-with-and-joins)).
- **Quoted names.** A table or column name in double quotes is the name inside them, so any name can be written: `"order"` (a word SQL reserves), `"doc no"`, `"say ""hi"""` (a quote doubled), and `FROM "papers"` reads papers. Python's [`table()`](python.md#queries-built-in-python) builds queries with no quoting at all.
- **Strings** are single-quoted, `'sale'`. Where a value goes, a double-quoted `"sale"` is a string too, as SQLite and MySQL read it, and a bare name is an error rather than a string: `WHERE tag = sale` says `no such column: sale`.
- **Clauses ChloeDB does not run are errors**, never ignored: `FETCH`, `FOR UPDATE`, `TOP`, `SELECT INTO`, `GROUP BY ... WITH ROLLUP` and the like. An error says where and why: the parser's give the line, the column and the token (`SQL syntax error at line 2, column 1 (near "FORM products"): ... found FORM (did you mean FROM?)`), and a misspelt column, table or function names the ones near it and what there is.
- **Conditions the indexes cannot answer** test each row the others find: a function or arithmetic of a column (`lower(brand) = 'acme'`, `price * 2 > 100`, `CAST(price AS INT) = 5`), two columns compared (`price > rating`), `LIKE` with `%` or `_` inside the pattern, `ILIKE`, `IS NULL` of a field whose index does not keep it, a `CASE`, and a comparison of a `TEXT` field, by its stored text (`title LIKE '%tent%'`, `title = 'Tent 2'`). A field with a [substring index](#substrings-like-and-ilike-anywhere-in-a-value) answers `LIKE` and `ILIKE` itself. The other conditions of the `WHERE` still narrow the scan by their indexes, so `year = 2020 AND title LIKE '%tent%'` reads the rows of 2020 only, and a `WHERE` of such conditions alone reads every row of the table. A field that is not stored (a generated `TEXT` field, an embedded vector) has no value to test.
- **`SELECT` of no table**, `SELECT 1 + 1` or `SELECT now()`, gives one row of its values.
- **`CAST(x AS type)`** (or `x::type`) makes an `INT` (rounded to the nearest whole number, as PostgreSQL rounds), a `FLOAT`, `TEXT`, and of a value a `BOOL` (`'true'`, `'yes'`, `1`) or a `TIMESTAMP` (`CAST('2026-11-02' AS TIMESTAMP)`); text that is not a number is an error.
- **`SELECT *`** returns every stored, non-vector column.
- **`score()`** is the value of the `ORDER BY` score for each row (over a `WITH` result read without an `ORDER BY`, the score the `WITH` ranked its rows by); **`hops()`** is a row's distance in a graph traversal ([below](#graphs)); **`seq_no()`** is the sequence number of the write that last changed the row, in a database that numbers its writes, for [conditional writes](server.md#conditional-writes). It is a column of a query on one table, and cannot be aggregated or ordered by. A [ranking expression](#features-the-values-a-ranking-read) that scores each row by itself is a column too (`bm25(title, :q) AS t`), and **`features()`** stands for the inputs of the query's ranking.
- **`ORDER BY`** takes one key, or several. The scan orders by the first and breaks its ties by a second that is the primary key (or, in a facet query, the group column); other keys sort the rows the `WHERE` keeps in memory, as over joined rows (`ORDER BY rating DESC, price`). Ranking by a score is always best first; a `KEYWORD` or stored `TEXT` column first sorts the rows by its text, `NULL` last either way, which the row evaluator does after reading every row the `WHERE` keeps; so does a function of a row's values without a score (`ORDER BY lower(brand)`, `ORDER BY date_trunc('day', at)`, `ORDER BY hash(id, 7)`); `ORDER BY distance(…) ASC` ranks by Euclidean distance, nearest first ([below](#similarityfield-vector-and-distancefield-vector)). `MAX k PER field` at its end keeps at most k rows of each value ([below](#at-most-k-rows-of-a-value-max-k-per)).
- **Parameters** are `:name` placeholders, bound from the API or from the shell's `--param name=value`. A vector parameter is a JSON array.
- **Limits.** An expression nests at most 1,000 levels, counting parentheses and the operators of a chain such as `a OR b OR …` (SQLite has the same default); a long list of values belongs in `IN (…)`, which has no such limit. A `LIMIT` larger than the table returns every row that qualifies. A query ranked by a score without a `LIMIT` returns 100 rows, counted after its `OFFSET`, on one database and through a coordinator alike, and says so: its stats hold `implicit_limit: 100`, and `truncated: true` with a plan line (`no LIMIT: ...`) when more rows matched (`result.truncated` and `result.implicit_limit` in Python, `result.truncated` and `result.implicitLimit` in JavaScript). A `DELETE` is never cut by that default: `DELETE ... ORDER BY <score>` without a `LIMIT` is refused, since its order would only choose which rows a `LIMIT` keeps; write the match in `WHERE` (`DELETE FROM t WHERE MATCH(body, 'x')`) to delete every row that matches. (Before, it deleted the top 100.)
- **Memory and time.** What a query builds up (its groups, `COUNT(DISTINCT)` and percentile values, the rows it orders before its `LIMIT` and the rows it returns, a graph walk's nodes, a compound query's parts and join tables) is counted against a memory budget as it grows, and a query that would go past it fails with a `memory:` error naming the budget, giving back what it held, instead of the process running out of memory. chloedb-server's queries share `--query-memory-mb` ([limits](server.md#limits)). A database opened in a program (Rust's `Db`, Python's `Database`, JavaScript's and Java's) counts its queries against one budget its whole process shares: a quarter of the machine's memory (the cgroup's limit, else RAM), or `CHLOEDB_QUERY_MEMORY_MB` MiB (`0`: none), read when the first database opens; a database or a query may be given a budget of its own (`memory` in [Python](python.md#limits-memory-deadlines-interrupting) and [JavaScript](javascript.md#limits-memory-deadlines-aborting), `ExecOptions::memory` in Rust). Such a database sets no deadline, no row window and no group cap of its own, unlike a server (`--timeout-ms`, `--max-result-window`, `--max-groups`): a query given a deadline (`timeout_ms`), or cancelled from another thread (`interrupt()`, an `AbortSignal`, `ExecOptions::cancel`), stops within a few thousand rows of it, checking between segments and every 1,024 rows it groups, orders, walks or joins.

## Columns and types

| type | indexed as | filters |
|---|---|---|
| `INT`, `FLOAT` (finite: no NaN or infinity) | frame-of-reference column with zone maps, range histogram | `=`, `!=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IS [NOT] NULL` |
| `KEYWORD` | dictionary codes and one docID list per value; with `substring = true`, a [substring index](#substrings-like-and-ilike-anywhere-in-a-value) too | `=`, `!=`, `IN`, `NOT IN`, `IS [NOT] NULL`, [prefixes](#prefixes-and-autocomplete) (`LIKE 'p%'`, `PREFIX`); `GROUP BY`; with a substring index, any `LIKE` and `ILIKE` |
| `BOOL` | an `INT` column of 1 and 0 | the field alone (`WHERE done`, `NOT done`), `=`, `!=`, `IS [NOT] TRUE`, `IS [NOT] FALSE`, `IS [NOT] NULL`; `GROUP BY` |
| `TIMESTAMP` | an `INT` column of microseconds since 1970-01-01 UTC | as `INT`, with times written as text: `at >= '2026-10-01'` |
| `TEXT` | inverted index (optionally with positions); may be generated from other text fields; with `substring = true`, a [substring index](#substrings-like-and-ilike-anywhere-in-a-value) too | `MATCH`; with a substring index, `LIKE` and `ILIKE` |
| `VECTOR(d)` | float32 rows plus 8-bit codes (or 1-bit codes, `"codes": "binary"`, or RaBitQ codes of 1, 2 or 4 bits, `"rabitq1"`, `"rabitq2"`, `"rabitq4"`), with HNSW, IVF or flat search; may be declared as a model's embedding of other fields ([embedded vector fields](server.md#embedded-vector-fields)) | ordering by `similarity` / `distance`; a similarity graph |
| `VECTOR(d)[]` | several vectors per row (a page's patches, a song's frames): 1-bit codes searched, 8-bit codes to score (float32 too with `keep_f32`), and maps from rows to their vectors and back | ordering by `maxsim` |
| `SPARSE` | inverted index of learned term weights (SPLADE and similar) | ordering by `similarity` |
| `MEDIA` | a file by reference: URI, SHA-256, type, size; a `KEYWORD` index of the hashes | `=`, `IN` on the hash; `GROUP BY` |
| `EDGES` | out-edge CSR and reverse index per segment | `REACHABLE`, `PATH`, `ppr` |
| `JSON` | an object's keys no schema names: one sorted dictionary of `path, type, value` terms with a docID list each ([JSON fields](#json-fields-keys-no-schema-names)) | on paths (`attrs.color`): `=`, `!=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN`, `NOT IN`, `LIKE 'p%'`, `PREFIX`, `IS [NOT] NULL`, `IS TRUE`, the path alone |

Predicates combine with `AND`, `OR`, `NOT`, [`AT_LEAST`](#some-of-the-conditions-at_least) and parentheses, across all of these, and [`max_rows`](#as-many-as-fit-min_rows-and-max_rows) lets an `AT_LEAST` choose its count.

A `KEYWORD` compares with text: `k = 1` is an error that points to `k = '1'`, as `n = '2'` is on an `INT` (a write still stores the number 1 in a `KEYWORD` as `'1'`). `INT` values are 64-bit integers and compare exactly: `id = 9007199254740993` finds that row and not 9007199254740992, which a double cannot tell apart. Arithmetic of `INT`s (`+`, `-`, `*`, `%`) is exact while its result fits in 64 bits; a result that does not is a `FLOAT`, not an error (`n * 9223372036854775807` of an `n` of 3 is `2.7670116110564327e19`), `/` always gives a `FLOAT`, and dividing by 0 (with `/` or `%`) gives `NULL`. A client that reads such a column as integers gets a float where a result overflowed. A fraction bounds integers by rounding inward (`n > 2.5` is `n >= 3`, `n = 2.5` matches nothing). A write gives an `INT` a float with no fraction as its whole number (`2.0` is 2, as pandas writes whole numbers in a column with gaps), from SQL, JSON and CSV alike, and refuses `2.5`.

`NULL` follows SQL's three-valued logic. A comparison with a missing value is neither true nor false, and so is its negation: `rating != 2` and `NOT rating > 2` leave out the rows without a rating. Ask for them with `rating IS NULL` (`IS NOT NULL` for the others). `MATCH` and graph predicates are always true or false, so `NOT MATCH(body, 'x')` includes rows without a body. A filter on a multi-valued field holds when some value matches, and its negation when none does, so `tags != 'red'` includes rows without tags (`tags IS NULL` finds those). `rating = NULL` is an error that points to `IS NULL`. `rating IN (1, NULL)` is TRUE where the rating is 1 and UNKNOWN elsewhere, so `rating NOT IN (1, NULL)`, like `NOT IN` a subquery that returns a `NULL`, keeps no row; a multi-valued field's `IN` is TRUE or FALSE (the `NULL` matches none of its values). `LIKE '%'` and `PREFIX(f, '')` hold for every row with a value and are UNKNOWN for the others, so their `NOT` keeps no row of a single-valued field. A scalar subquery that finds no row is `NULL`, and a comparison with it keeps no row either way.

A `BOOL` is `TRUE` or `FALSE`; a write also takes 1 and 0 and the text `'true'`, `'false'`, `'t'`, `'f'`, `'1'` and `'0'`, and refuses anything else. A field named alone is a condition (`WHERE done AND NOT archived`), with `NULL` neither true nor false: `NOT done` and `done != TRUE` leave out the rows without a value, `done IS NOT TRUE` takes them in. `SUM(done)` counts the trues, and answers come back as booleans.

A `TIMESTAMP` is a moment, kept to the microsecond in UTC and written as ISO 8601 text: a date (`'2026-10-01'`, midnight UTC) or a date and time (`'2026-10-01T12:30:00Z'`, `'2026-10-01 14:30:05.25+02:00'`), with an offset or none (UTC). A number is refused, so seconds are not mistaken for microseconds. Comparisons, `BETWEEN`, `IN`, `ORDER BY`, `MIN`, `MAX` and `COUNT` work on it (`SUM` and `AVG` do not), and answers are ISO 8601 text in UTC, such as `2026-10-01T12:30:05.250Z`. In `decay(at, origin, scale)` on a `TIMESTAMP`, the origin is a time or `now()`, and the scale and offset are seconds: `decay(published, now(), 2592000)` halves a row's weight for each 30 days of age. `date_trunc`, `EXTRACT` and `strftime` take [parts of times](#parts-of-times-date_trunc-extract-and-strftime), for `GROUP BY` a day, a week or a month, and [intervals](#times-later-and-earlier-interval) move a time (`now() - INTERVAL '15 minutes'`). A `DATE` column is a `TIMESTAMP`: a date written alone is its midnight UTC, and a time given is kept. A database whose tables use `BOOL` or `TIMESTAMP` is written in [file format 3](index.md#wal-memtable-and-snapshots), which versions of ChloeDB before these types refuse.

`KEYWORD` and `INT` fields may be multi-valued (`"multi": true` in the schema): a row holds a list, such as `ARRAY['red', 'blue']`, and a filter matches a row if any of its values matches. A facet counts each distinct value once per row.

A multi-valued field also compares with a list of values as a set, by PostgreSQL's array operators. The list is `ARRAY[...]` of values or a list parameter (`:groups`), and its values are written as the field's are:

| condition | holds where | read from the index as |
|---|---|---|
| `tags @> ARRAY['red', 'sale']` | the row has every value of the list (contains all) | an `AND` of equalities; every row for an empty list |
| `tags && ARRAY['red', 'sale']` | the row has a value of the list (overlaps) | `IN` the values; no row for an empty list |
| `groups <@ :allowed` | each value of the row is in the list (contained by), on an `INT[]` field | `NOT` of the ranges between the list's values |

The list may come first: `ARRAY['red'] <@ tags` is `tags @> ARRAY['red']`. A row without values is the empty set, so `@>` an empty list and `<@` any list hold for it, and `NOT` of a set operator holds where the set operator does not, as for other conditions on a multi-valued field. `<@` is how a row lists what it needs and a query what it has: with `required INT[]` holding the groups a reader must belong to, `WHERE required <@ :user_groups` keeps the documents whose every required group is one of the user's (and those that require none), and it filters a vector or text search as any condition does. A `KEYWORD[]` field does not take `<@` yet, since its index cannot list the values outside a list; an `INT[]` of ids does. Over rows in memory (a `WITH` result), `&&` compares any two lists, and `@>` a list column with a list written out.

A `KEYWORD[]` field may hold the [semantic ID](server.md#semantic-ids) of a vector field: every prefix of a code such as `12.7.201.0`, filled in by the writer, so `sid = '12.7'` finds the rows under a prefix. `sid_at(sid, n)` is the first `n` numbers of a code (`sid_at(sid, 2)` is `'12.7'`), in `SELECT`, `WHERE` and `GROUP BY`.

A `SPARSE` value is a JSON object of terms and non-negative weights, `{"tok": 1.25, "##en": 0.4}`, as a learned sparse model (SPLADE, BGE-M3's sparse output, OpenSearch's neural sparse models) writes it. Terms are taken as given, not analysed, and weights are stored in steps of 1/1024.

A `MEDIA` value names a file, `{"uri": "https://cdn.example.com/a.png", "sha256": "…", "mime": "image/png", "bytes": 48213, "width": 640, "height": 480}`, and a write may give only its URI, `'https://cdn.example.com/a.png'`, or its bytes, `'{"data": "<base64>"}'`, for the server to fill in ([media fields](server.md#media-fields)). Its index holds the hash: `image = '9f86d0…'` finds the rows of that file. `OPTIONS(kind = 'image')` takes only images (`'audio'`, `'video'`, `'pdf'`), whose vectors a `VECTOR` field can hold: `emb VECTOR OPTIONS(embed = 'image', model = 'siglip-base-patch16-224')` ([vectors of images](server.md#vectors-of-images)), `sound_emb VECTOR OPTIONS(embed = 'recording', model = 'clap-htsat-unfused')` ([vectors of sound](server.md#vectors-of-sound)), and whose captions or transcripts a `TEXT` field can: `caption TEXT OPTIONS(derive = 'image', model = 'blip-image-captioning-base')`, `transcript TEXT OPTIONS(derive = 'recording', model = 'whisper-small')` ([text from media](server.md#text-from-media)). A language model can make a `TEXT` field of another's text, told what by a prompt: `product TEXT OPTIONS(derive = 'body', model = 'anthropic/claude-haiku-5-5', prompt = 'Name the product this review is about.')` ([text from text](server.md#text-from-text)).

### Parts of times: `date_trunc`, `EXTRACT` and `strftime`

```sql
SELECT date_trunc('day', at) AS day, COUNT(*), AVG(price), MIN(price) FROM snapshots GROUP BY day ORDER BY day;
SELECT EXTRACT(dow FROM at) AS dow, AVG(price) FROM snapshots WHERE item = 'i42' GROUP BY dow;
SELECT strftime('%Y-%m', at) AS month, SUM(price) FROM sales GROUP BY month;
SELECT date_bin('5 minutes', ts) AS b, severity, COUNT(*) FROM logs WHERE ts >= '2026-11-12' GROUP BY b, severity;

-- A moving average of daily prices: groups by day in a WITH, a window over them.
WITH daily AS (SELECT item, date_trunc('day', at) AS day, AVG(price) AS px FROM snapshots GROUP BY item, day)
SELECT item, day, px, AVG(px) OVER (PARTITION BY item ORDER BY day ROWS BETWEEN 6 PRECEDING AND CURRENT ROW) AS ma7
FROM daily;
```

- **`date_trunc(unit, t)`** is the time cut to the start of its `second`, `minute`, `hour`, `day`, `week` (from Monday, as ISO 8601 and PostgreSQL count weeks), `month`, `quarter` or `year`, a `TIMESTAMP`. The unit is text in any case, and a plural (`'days'`) is taken too.
- **`date_bin(width, t [, origin])`** is the start of the bucket of a fixed `width` that the time is in, a `TIMESTAMP`, as PostgreSQL's `date_bin`: buckets are counted from `origin` (a `TIMESTAMP` or ISO 8601 text; 1970-01-01 without one), so `date_bin('15 minutes', at, '2026-01-01T00:05:00Z')` starts buckets at :05, :20, :35 and :50. The width is a whole number and a unit: `microseconds` (`us`), `milliseconds` (`ms`), `seconds` (`s`), `minutes` (`m`), `hours` (`h`), `days` (`d`) or `weeks` (`w`), singular or plural, as `'5 minutes'`, `'30s'` or `'1 day'`; months and years are not a fixed width, and are `date_trunc`'s. `date_bin('1 day', t)` is `date_trunc('day', t)`. OpenSearch's `date_histogram` with a `fixed_interval` runs as one.
- **`date_part(part, t)`**, and **`EXTRACT(part FROM t)`** which is the same function, is a number: `year`, `isoyear`, `quarter` (1 to 4), `month` (1 to 12), `week` (the ISO week, 1 to 53), `day` (of the month), `doy` (of the year, 1 to 366), `dow` (0 for Sunday to 6 for Saturday, as PostgreSQL counts), `isodow` (1 for Monday to 7 for Sunday), `hour`, `minute`, and as a `FLOAT`, `second` (with its fraction) and `epoch` (seconds since 1970).
- **`time_bucket(width, t)`** is another name for `date_bin(width, t)`, as TimescaleDB spells it: `time_bucket('5 minutes', ts)` gives 03:00, 03:05, 03:10. The width can also be an `INTERVAL '5 minutes'`.
- **`strftime(format, t)`** is text, written as SQLite's `strftime` writes it: `%Y` year, `%m` month 01-12, `%d` day 01-31, `%H` hour 00-23, `%M` minute, `%S` second, `%f` seconds with milliseconds (`SS.SSS`), `%j` day of the year 001-366, `%w` day of the week 0-6 from Sunday, `%u` 1-7 from Monday, `%W` week of the year 00-53 from the first Monday, `%V` the ISO week 01-53 and `%G` its year, `%s` seconds since 1970, `%F` for `%Y-%m-%d`, `%T` for `%H:%M:%S`, and `%%` for a `%`.

- **A day.** `CAST(t AS DATE)`, `DATE(t)` and `date_trunc('day', t)` are the same, the start of the time's day; ChloeDB keeps it a `TIMESTAMP` (there is no separate `DATE` type). **`CURRENT_DATE`** is the start of today, `date_trunc('day', now())`, so `WHERE at >= CURRENT_DATE - INTERVAL '7 days'` keeps the last seven days and today, and `GROUP BY CAST(at AS DATE)` counts by day.

They all read times in UTC. A `NULL` time gives `NULL`, text is read as a time (`date_trunc('month', '2026-11-05')`), and any other value is an error, as are a unit, part or directive they do not have (the message lists the ones they do).

A **`GROUP BY` of one of them over a `TIMESTAMP` field of the table** (indexed, one value per row) runs in the executor, as a `GROUP BY` of a column does: it reads the field's column, has no limit on the number of rows, and over shards each shard groups its own rows and the coordinator merges the groups. The rows without a time form the group `NULL`. Anywhere else, in `SELECT`, `WHERE` and over joined or derived rows, they are computed row by row; in `WHERE` they are a [condition the indexes cannot answer](#statement-shape), so a range of times is faster written on the field (`at >= '2026-11-01' AND at < '2026-12-01'`) than as `EXTRACT(month FROM at) = 11`. A plan that uses them needs the plan feature `dates`, which shards of a version without these functions refuse, and one with `date_bin` also needs the plan feature `date_bin`.

### Samples, splits and shuffles: `hash`

```sql
-- about a tenth of the rows, the same tenth every time
SELECT * FROM events WHERE hash(id, 7) % 10 = 0;
-- a train/test split, 80/20: every row in one part
SELECT * FROM events WHERE hash(id, 7) % 10 < 8;    -- train
SELECT * FROM events WHERE hash(id, 7) % 10 >= 8;   -- test
-- the same thousand rows in the same random order
SELECT * FROM events ORDER BY hash(id, 7) LIMIT 1000;
```

**`hash(x [, seed])`** is an `INT` from 0 to 2<sup>63</sup> - 1 that only `x` and `seed` (an `INT`, 0 without one) decide: the same on every machine, in every version of ChloeDB, and on a file and over shards, so a sample or a split made with it is the same rows wherever and whenever it runs. Another seed gives another sample. A `NULL` value or seed gives `NULL`, so a row whose `x` is `NULL` is in no part; hash a column every row holds, such as the key. `x` is one value: text, a number, a `BOOL` or a `TIMESTAMP` (a list is an error). To hash several columns, chain them through the seed: `hash(item, hash(user, 7))`.

It is the 64-bit FNV-1a of the seed's 8 bytes (little-endian) and then the value's, through SplitMix64's finalizer, shifted right by one. A text's bytes are its UTF-8; an `INT`, a `BOOL` (1 or 0) and a `TIMESTAMP` (microseconds since 1970) are their integer's 8 bytes, little-endian; a `FLOAT` is its IEEE 754 bits, little-endian (-0 as 0). So it is the same value elsewhere, in Python for instance:

```python
def chloedb_hash(data: bytes, seed: int = 0) -> int:     # data: "u42".encode(), (42).to_bytes(8, "little", signed=True)
    m, h = (1 << 64) - 1, 0xcbf29ce484222325
    for b in seed.to_bytes(8, "little", signed=True) + data:
        h = ((h ^ b) * 0x100000001b3) & m
    h = ((h ^ (h >> 30)) * 0xbf58476d1ce4e5b9) & m
    h = ((h ^ (h >> 27)) * 0x94d049bb133111eb) & m
    return (h ^ (h >> 31)) >> 1
```

In `WHERE`, `hash` is a [condition the indexes cannot answer](#statement-shape): the other conditions narrow the rows by their indexes, and those rows are hashed one by one. `ORDER BY hash(id, 7)` sorts the rows by it, as `ORDER BY lower(name)` or a part of a time sorts them: the rows the table's query finds, in memory. A plan that uses it needs the plan feature `hash`, which shards of a version without the function refuse. A [frame](frames.md) samples and splits with it: `frame.sample(fraction=0.1, seed=7)`, `train, test = frame.split(0.8, seed=7)`.

### JSON fields: keys no schema names

A `JSON` field holds an object whose keys are not declared, for attributes that only a few rows have and that nobody knew when the table was made: a product's `voltage`, `certs` or `dims.w`. A row adds keys by holding them, and filters on any key run from the field's index:

```sql
CREATE TABLE products (id KEYWORD PRIMARY KEY, title TEXT, attrs JSON);
INSERT INTO products (id, title, attrs) VALUES
  ('p1', 'charger', '{"voltage": 5, "certs": ["CE", "UL"], "dims": {"w": 40.5}}'),
  ('p2', 'lamp', '{"color": "black", "a.b": true}');

SELECT id FROM products WHERE attrs.voltage >= 5 AND attrs.certs = 'UL';
SELECT id, attrs.dims.w FROM products WHERE attrs.color IN ('black', 'red') ORDER BY attrs.dims.w DESC;
SELECT attrs.color, COUNT(*) FROM products GROUP BY attrs.color;
```

- **Paths.** `attrs.dims.w` is the key `w` of the object at `dims`; `attrs['a.b']` names a key that holds a dot, and `json_extract(attrs, 'dims.w')` is the same path as a function (`'$.dims.w'` too). In a join, qualify the field: `p.attrs.color`. An array holds its elements at its own path, as a multi-valued field does: `attrs.certs = 'UL'` matches `["CE", "UL"]`, and `{"items": [{"sku": "x"}, {"sku": "y"}]}` holds `x` and `y` at `items.sku`. JSON `null` and an empty array are no value.
- **Types.** A comparison sees the values of its literal's type only, as if the path were a column of that type: `attrs.voltage > 5` reads the numbers at `voltage` (integers and doubles alike), `attrs.voltage = 'n/a'` its strings, `attrs.ok` and `attrs.ok = TRUE` its booleans. A row with no value of that type there is `NULL` to the comparison, so `NOT (attrs.voltage > 5)` and `attrs.voltage != 5` leave out rows whose voltage is missing or a string, and `IS NULL` finds rows without the path at all. An `IN` list of several types reads each type for its own values. No row is refused for holding a type other rows do not.
- **Reads.** `SELECT attrs` returns the object as compact JSON text with sorted keys. A path in `SELECT`, `ORDER BY`, `GROUP BY` or an aggregate returns its value, in a column named as the query writes the path (`attrs.dims.w`): a string, an integer, a double or a boolean; a list for an array of values or for several values through an array; an object as its JSON text; `NULL` where the row has none (an empty array included). A list makes a group per value, as a multi-valued column does, and a boolean is a group apart from the number 1.
- **Groups, aggregates and sorts read the index.** `GROUP BY attrs.color`, `AVG(attrs.price)` and `ORDER BY attrs.price` read each row's value from the field's terms, as a declared column's are read, so they take no row's object: a path's counts come from its terms' docID lists, as a facet's do. A row holds one value term per path; where that term is not the whole of the value (an object, an array, a string longer than 256 bytes, a number such as `5.0` or 2^53 + 1 that its term would give as another), the index marks the row, and those rows alone are read from their objects. On a million rows with 1,000 sparse keys (a 4-core cloud VM, [`chloedb/results/json`](https://github.com/chloetech-ai/chloedb/tree/main/chloedb/results/json)), `GROUP BY` the most held key takes 2.7 ms, against 2.4 ms for a declared column and 1.9 s when each row's object was read, and `AVG` of a number key held by 82,000 rows 22 ms, against 14 ms and 552 ms. Selecting a path reads it from the objects of the rows returned. A query that mixes paths with what the executor does not run itself (a join, a function of a path, `GROUP BY lower(attrs.color)`) reads them from the rows' objects instead, in memory (up to a million rows), as over [joined rows](#subqueries-with-and-joins); the answer is the same either way.
- **Writes.** A value is a JSON object, given as text (`'{"k": 1}'`), as an object in JSON Lines or the HTTP API, or as a dict from Python; `NULL` is no object. A write is refused, naming the limit, for anything else, for an empty key, and for an object nested more than 16 levels, with a path longer than 256 bytes or with more than 1,000 values and objects. A string longer than 256 bytes is kept and returned, and `IS NOT NULL` sees it, but comparisons do not: its value is not indexed (its words are, with [text](#json-fields-keys-no-schema-names)).
- **Text.** `attrs JSON OPTIONS(text = true)` also indexes the strings of each row's object as text, one a line in path order, long ones included, so `MATCH(attrs, 'charger NOT lamp')` finds rows by the words of any of their strings and `bm25(attrs, 'usb charger')` ranks them as it ranks a `TEXT` field holding the same lines (with a cluster's statistics through a coordinator). Keys, numbers and booleans are not text: a path reads them. The option takes an `analyzer` (`'english'`, the default, or another a `TEXT` field takes), and the text has positions, so a phrase may span two strings that follow each other. A database with such a field needs the feature `json_text` besides `json`.
- **The index** is one sorted dictionary per segment of terms `path \0 type value` (strings as UTF-8, numbers in an order-preserving encoding of doubles), so `=` and `IN` read one term's docID list, a range or `LIKE 'p%'` one run of terms, and `IS NOT NULL` the path's own term. A row pays only for the paths it holds, which is what keeps thousands of sparse keys cheap where as many declared columns would each take a slot in every row. A `JSON` field is stored; it cannot be the primary key, multi-valued, have a default, or be generated or the source of a generated or embedded field. A database with a `JSON` field is written in [file format 3](index.md#wal-memtable-and-snapshots) with the feature `json`, and plans that read paths carry the plan feature `json`, and `json_columns` when a Search groups, aggregates, orders or selects by them, which a version of ChloeDB from before it refuses by name. A segment written before marks (its section `json.<field>.marks` missing) reads every row holding the path from its object, until a merge rewrites it.

#### Which paths a field holds: `json_paths`

Nobody declared the keys, so the table says what they are. `json_paths` lists every path a `JSON` field's objects hold, with how many rows hold each, the types of its values and what they are, read from the field's index (its terms and their docID lists) without reading a row:

```sql
SELECT * FROM json_paths('products', 'attrs');
SELECT * FROM json_paths('products', 'attrs', 'dims');                  -- dims and the paths below it
SELECT * FROM json_paths('products', 'attrs', filter => 'MATCH(title, ''charger'')');
```

For the two rows above, the first gives:

| path | rows | share | strings | numbers | bools | min | max | distinct | values |
|---|---|---|---|---|---|---|---|---|---|
| `a\.b` | 1 | 0.5 | 0 | 0 | 1 | | | 1 | `[[true,1]]` |
| `certs` | 1 | 0.5 | 1 | 0 | 0 | | | 2 | `[["CE",1],["UL",1]]` |
| `color` | 1 | 0.5 | 1 | 0 | 0 | | | 1 | `[["black",1]]` |
| `dims` | 1 | 0.5 | 0 | 0 | 0 | | | 0 | `[]` |
| `dims.w` | 1 | 0.5 | 0 | 1 | 0 | 40.5 | 40.5 | 1 | `[[40.5,1]]` |
| `voltage` | 1 | 0.5 | 0 | 1 | 0 | 5.0 | 5.0 | 1 | `[[5,1]]` |

- **A row per path**, in path order, a key's dot escaped (`a\.b`). `rows` counts the rows that hold the path at all (an object there counts), `share` is their share of the rows read, and `strings`, `numbers` and `bools` count the rows holding a value of each type, so a path whose rows disagree on its type says so. `min` and `max` are its numbers' range; `distinct` counts its values, and `values` lists them as JSON with their rows, most first, when there are at most 100 (past that both are `NULL`).
- **The third argument** narrows the listing to a path and the paths below it (`'dims'`, `'$.dims'`, `'a\.b'`). **`filter =>`** counts only the rows a condition keeps, any condition a `WHERE` could hold: `json_paths('products', 'attrs', filter => 'attrs.color IS NOT NULL')` tells which other keys the rows with a color have.
- **The listing is read whole**: `SELECT *` and nothing else, at most 10,000 paths (past that, the first 10,000 in order). A string longer than 256 bytes is not indexed, so it counts at its path in `rows` but in no type. Rows deleted or updated count as they are now.
- **Over HTTP**, `GET /v1/tables/T/paths?field=attrs&path=dims&where=...` gives the same as JSON, each path with its `keys`, and a [coordinator](server.md#sharding-one-coordinator-many-shards) sums its shards' listings, so the counts are the cluster's. The [command center](server.md#the-ui) draws the listing as a tree in **Schema**, completes `attrs.` in the workbench with the keys at each level, and lists the paths in the query builder as fields to filter, group, measure and sort by.

## Some of the conditions: `AT_LEAST`

`AT_LEAST(n, c1, c2, …)` holds where at least `n` of the conditions do. A condition is anything `WHERE` takes: a comparison, `IN`, `IS NULL`, `MATCH`, a graph predicate, a subquery condition, a group in parentheses, another `AT_LEAST`.

```sql
-- two of three signals, ranked by the text
SELECT id, title FROM papers
WHERE AT_LEAST(2, MATCH(title, 'covid'), year >= 2020, venue IN ('nejm', 'lancet'))
ORDER BY bm25(body, :q) DESC LIMIT 20;

-- rows meeting more of the conditions rank higher
SELECT id FROM papers
ORDER BY bm25(body, :q) * (1 + 0.1 * count_true(year >= 2021, venue IN ('nejm', 'lancet', 'jama'))) DESC LIMIT 20;
```

- **`n`** is a count, a percentage of the conditions (`'75%'`, rounded down) or a parameter (`:n`), 1 or more (`0` and `'0%'` are errors: none of the conditions is no condition). Either is clamped to between 1 and the number of conditions, as [`MATCH`'s groups](#full-text-match) are: `AT_LEAST(1, …)` is the `OR` of the conditions, and a count past the end is their `AND`.
- **`NULL`.** A condition that is UNKNOWN (a comparison with a `NULL`) is not met. Under `NOT`, `AT_LEAST` follows SQL's three-valued logic: `NOT AT_LEAST(n, …)` holds where fewer than `n` conditions would be TRUE even if every UNKNOWN one were, so it never returns a row whose answer depends on a `NULL`. With `a = 1` and `c` `NULL`, `AT_LEAST(2, a > 0, c = 'x')` leaves the row out, and so does `NOT AT_LEAST(2, a > 0, c = 'x')`.
- **`count_true(c1, c2, …)`** is how many of the conditions hold, a number for [ranking expressions](#ranking-expressions) and `SELECT`, exactly the sum of `CASE WHEN c THEN 1 ELSE 0 END` over them: a condition on a `NULL` counts 0. In a `SELECT` over rows in memory (a join, a `WITH` result) its conditions cannot use `MATCH` or graph predicates, which only a table's indexes answer.
- **How it runs.** Each condition becomes the set of rows it holds for, as `WHERE` evaluates it, and a counter adds the sets up 64 rows to a machine word, in ⌈log2(y + 1)⌉ words for each 64 rows; sets with few rows are merged instead. A ranking that checks its candidates one at a time tests their conditions in turn, until the count is reached or out of reach. The cost grows with the number of conditions, not with `n`: over 1,000,000 rows where each condition holds for an independent 20% of them, counting the rows took 3.0 ms for 2 of 4 conditions, 4.1 ms for 4 of 8 and 9.3 ms for 8 of 16, against 0.37 ms for one condition. The same conditions written as an `OR` of every `AND` of `n` took 8.2 ms for 2 of 4 (6 `AND`s) and 114 ms for 4 of 8 (70), and 8 of 16 would need 12,870 (`chloedb-at-least-bench` on a 4-core cloud VM, Intel Xeon @ 2.10GHz, `chloedb/results/strata/at_least.json`).
- A plan with `AT_LEAST` or `count_true` needs the plan feature `at_least`. From Python, [`at_least(2, match("title", "covid"), col("year") >= 2020)`](python.md#queries-built-in-python) and `count_true(…)` build them.

## As many as fit: `min_rows` and `max_rows`

An "x of y" condition can choose its own x. With `max_rows` or `min_rows`, an `AT_LEAST`, a `MATCH(…, min => n)` or a `bm25(…, min => n)` asks for all y of its items first, and relaxes towards `n` while the rows are too few, stopping before a level that would match more than `max_rows`:

```sql
-- as many of the signals as keep the candidates to 2,000 or fewer
SELECT id, title FROM papers
WHERE year >= 2015 AND AT_LEAST(2, MATCH(title, 'covid'), venue IN ('nejm', 'lancet'), cited > 100, max_rows => 2000)
ORDER BY bm25(body, :q) DESC LIMIT 20;

-- all four words if 50 rows have them, else three, else two
SELECT id, score() FROM docs
ORDER BY bm25(body, 'mrna vaccine trial phase', min => 2, min_rows => 50) DESC LIMIT 10;
```

- **The bounds** are counts of rows, 1 or more, and `min_rows` is at most `max_rows` (else no level could meet both, an error).
- **The level.** From the strictest level, all y items, a guard relaxes one item at a time while the level matches fewer than `min_rows` rows and the next looser one no more than `max_rows`, down to `n`. With `max_rows` alone that is the loosest level that fits, and with `min_rows` alone the strictest with enough rows. If even all y match more than `max_rows`, the query runs at y. The rows counted are those the rest of `WHERE` keeps; `LIMIT` does not cut them.
- **Where.** A guard is an `AT_LEAST` or a `MATCH` joined to the rest of `WHERE` by `AND` (not one under `OR`, `NOT` or another condition), or a `bm25` of the first ranking of `ORDER BY`. `MATCH` and `bm25` need a `min`, the loosest level; `AT_LEAST`'s is its `n`. A query has one guard at most, and a [rerank phase](#rerank-phases) none. A `SELECT` over a `WITH` result that is not a rerank phase counts that result's rows.
- **The plan line** gives each level's rows and the level chosen: `guard: AT_LEAST 3 of 4 (4: 12, 3: 340, 2: 5120, 1: 23000; max_rows 2000)`.
- **Its cost** is counting the rows of every level, over all the rows the rest of `WHERE` keeps, before running at one: a guarded query costs about what counting its matches does, whatever its `LIMIT`. Over 1,000,000 rows where each condition holds for an independent 20% of them, `AT_LEAST(1, …, max_rows => 10000) LIMIT 100` took 2.5 ms over 4 conditions, 6.1 ms over 8 and 11.6 ms over 16, against 0.25, 0.51 and 0.98 ms for the same query written at the level chosen. For four words of different frequencies, over rows of 12 words drawn by Zipf's law from 5,000, `bm25(body, …, min => 1, max_rows => 1000)` took 2.4 ms at the median of 50 queries (`LIMIT 10`), against 1.4 ms at the level chosen and 0.75 ms without `min`, and a `MATCH` guarded alike 2.3 ms against 1.3 ms. A text's words are counted once each, as `AT_LEAST`'s conditions are, unless it has required or excluded words, when each level is counted in turn (`chloedb-guard-bench` on a 4-core cloud VM, Intel Xeon @ 2.10GHz, `chloedb/results/strata/guard.json`).
- **What it keeps.** BEIR's queries are sentences, and few rows hold all of a sentence's words. On five BEIR collections the top 100 rows by `bm25(body, :q, min => '100%')` held 0.03 to 0.40 of the relevant rows (recall@100), against 0.13 to 0.97 for any of the words; `min => 1, min_rows => 100`, all the words while 100 rows have them, else as many as 100 rows have, held 0.10 to 0.96, within 0.4 to 7.7 points of any word, with nDCG@10 within 0.013 of it, at a median of 0.15 to 2.05 ms a query, 0.8 to 1.6 times the any-word query's (`chloedb-bm25-eval`, the `_r100` variants, 3 runs each on a 4-core cloud VM, Intel Xeon @ 2.10GHz, `chloedb/results/text/bm25_guard.json`).
- **Over shards**, a coordinator first asks every shard for its counts (protocol 8, so a shard of an older server is refused by name), adds them up, chooses the level as one database would, and sends the shards the query at that level: one round more. A shard answering a coordinator runs the level it is sent, never one of its own. With rows like those above split over 4 shards on one machine, the guarded `AT_LEAST` of 4, 8 and 16 conditions took 4.3, 7.4 and 12.6 ms through the coordinator (1.6, 2.6 and 1.8 ms at the level chosen) against 2.3, 3.9 and 7.7 ms on one database, and the guarded `bm25` 4.3 ms against 1.7 ms, medians of 50 queries (`chloedb-guard-shards-bench`, the same VM, `chloedb-server/results/guard/shards-4.json`).
- A plan with a guard needs the plan feature `guard`. From Python, `at_least(2, …, max_rows=2000)`, `match(field, query, min=2, max_rows=500)` and `bm25(field, text, min=2, min_rows=50)`.

## Full text: `MATCH`

`MATCH(field, query)` is a boolean predicate. The query grammar follows SQLite FTS5:

```text
query   := and ("OR" and)*
and     := unary (["AND"] unary)*          juxtaposition is AND
unary   := ("NOT" | "-") unary | primary    "a NOT b" is "a AND NOT b"
primary := "(" query ")" ["@" n | "@" p "%"] | ("\"phrase\"" | word | word* | word~ | word~N) ["^" weight]
```

| query | matches |
|---|---|
| `vaccine mrna` | both words (juxtaposition is `AND`) |
| `vaccine OR vaccination` | either |
| `vaccine -animal`, `vaccine NOT animal` | the first without the second |
| `"messenger rna"` | the phrase (needs positions on the field) |
| `vacc*` | every indexed term with that prefix |
| `vacine~`, `vacine~1` | the word and the indexed terms an edit or two from it ([typos](#typos-and-synonyms)) |
| `vaccine AND (mrna OR "messenger rna") -animal` | any combination, with parentheses |
| `(mrna vaccine trial phase)@2`, `(…)@50%` | at least 2 of the group's items, or half of them |

Operators are uppercase; lowercase `and`, `or` and `not` are ordinary words (and stopwords). Parentheses and `NOT`s nest at most 100 deep. Words and phrases are analysed like the field's documents, by its [analyzer](#tables): a word that analyses to several terms (`e-mail`) becomes a phrase, and one that analyses to none (a stopword) is dropped. A prefix is lowercased and folded but not stemmed, and matches the stemmed terms that start with it. A weight (`vaccine^2`, `"messenger rna"^0.5`) is read and ignored, since `MATCH` only says whether a row matches: `vaccine^2 mrna` matches what `vaccine mrna` does, so one text can go to both `MATCH` and [`bm25`](#weights-required-and-excluded-words). A weight of 0 is read and ignored too, while `bm25` scores nothing for that word. A `^` before something that starts like a number but is not a weight (`vaccine^1e308`, `vaccine^-1`, `vaccine^2x`), and a `~` with more than 2 typos (`apple~9`), are errors in `MATCH`, where they would otherwise be searched as words; `bm25` reads its text leniently, and there they are words.

A group with `@n` matches a row with at least `n` of its items: its operands, whether juxtaposed, joined by `AND` or by `OR`, each a word, a phrase, a prefix, a typo-tolerant word or a parenthesised query (`vaccine AND (mrna "messenger rna" trial~ (phase OR stage))@2`). A negated item (`-animal`) is not counted: the row must not have it. An item written twice is one item, as in `bm25`'s `min` (`(apple apple cherry)@2` needs both words), and so are words that analyse to the same term. A stopword is not an item, and a word's synonyms and typo neighbours count as it does. `@p%` asks for p percent of the items, rounded down; a count or percentage of 0 is an error, and either is clamped to between 1 and the number of items, as Lucene's `minimum_should_match` is, so `@1` is any of them and a count past the end is all of them. `MATCH(body, 'a b c d', min => 2)` (or `min => '75%'`) treats the whole query as one group. A group nests anywhere a parenthesised query does. A group reads each item's matches once, so it costs about what an `OR` of its items does.

## Prefixes and autocomplete

`kw LIKE 'p%'` and `PREFIX(kw, 'p')` select the rows of a `KEYWORD` field whose value starts with `p`, byte for byte (no case folding). A field's values are sorted in its dictionary, so the values with a prefix are one run of it, found by two binary searches per segment: a short run unions its values' lists, and one of more than 1,024 values of a single-valued field scans the code column, as a range of numbers does. A multi-valued field matches a row when one of its values starts with the prefix, and `NOT` when none does.

```sql
-- autocomplete: the ten most asked queries that start with what was typed
SELECT id, n FROM queries WHERE PREFIX(id, :typed) ORDER BY n DESC, id LIMIT 10;

-- a user's own history first: keys of the form '<user>|<query>'
SELECT id, n FROM user_queries WHERE PREFIX(id, 'u42|how to') ORDER BY n DESC LIMIT 3;

-- with typos: 'hwo to ins' still finds 'how to install'
SELECT id, n FROM queries WHERE PREFIX(id, :typed, fuzzy => 'auto') ORDER BY n DESC, id LIMIT 10;

-- the same, exact completions first, then those one edit away, then two
SELECT id, n FROM queries WHERE PREFIX(id, :typed, fuzzy => 2)
ORDER BY CASE WHEN PREFIX(id, :typed) THEN 2 WHEN PREFIX(id, :typed, fuzzy => 1) THEN 1 ELSE 0 END * 1000000 + n DESC LIMIT 10;

SELECT id FROM files WHERE path LIKE 'src/query/%' AND path NOT LIKE 'src/query/ir/%';
```

- `LIKE` on a table's own `KEYWORD` field reads its index for a value (no wildcard: `=`) or text followed by `%`. Another pattern (`_` anywhere, or `%` before the end) and `LIKE` on a `TEXT` field test each row the other conditions keep, as over joined or derived rows. A field with a [substring index](#substrings-like-and-ilike-anywhere-in-a-value) answers every pattern from it.
- `ESCAPE 'c'` makes the character after `c` literal: `LIKE 'snake!_%' ESCAPE '!'` is the prefix `snake_`. `PREFIX` needs no escaping.
- `ILIKE` tests each row the other conditions keep. To find values in any case among many rows by an index, give the field a [substring index](#substrings-like-and-ilike-anywhere-in-a-value), or store the value lowercased (and a display form beside it) and use `LIKE`.
- `PREFIX(x, p)` also works over joined or derived rows, as a test of each value; `PREFIX(kw, '')` and `LIKE '%'` are `IS NOT NULL`.
- **`PREFIX(kw, p, fuzzy => 1 | 2 | 'auto')`** also selects the values that start with a prefix within that many edits of `p` whose first letter is `p`'s own, with the edits `MATCH` counts ([typos](#typos-and-synonyms)); `'auto'` goes by `p`'s length, as it does for a word: none up to 2 letters, 1 up to 5, else 2. The dictionary is walked as a trie, one row of the edit-distance table per letter, jumping past every value whose start is already too far from `p` and past the whole run of a start close enough, so the cost follows the values near what was typed rather than the dictionary's size. The values found are a few runs of the dictionary, read as an exact prefix's one is. A value's edits are the fewest of any of its prefixes, so a completion of `p` itself has none even when a shorter prefix is also close enough. Typo matches rank by the `ORDER BY` like exact ones; to rank completions by their edits, exact ones first and each tier by its count, put the tiers in the ranking expression: `ORDER BY CASE WHEN PREFIX(id, :typed) THEN 2 WHEN PREFIX(id, :typed, fuzzy => 1) THEN 1 ELSE 0 END * 1000000 + n DESC` (the factor larger than any count). `LIKE` takes no typos.
- Counts kept with `UPDATE ... SET n = n + 1` or a write's `add` change in place, so a query log's popularity stays current without rewriting rows.

A plan with a prefix needs the plan feature `prefix`, and one with typos also `prefix_edits`, so a shard of an older version refuses it rather than misreading it. The same run of the dictionary answers [semantic-ID next codes](server.md#semantic-ids).

**Cluster a completion table by its key.** `ORDER BY n DESC LIMIT 10` reads the count column's blocks best first ([ordering by a column](#a-column)), and with `OPTIONS(cluster_by = 'id')` a prefix's rows sit together in each segment, so the ten most asked queries under a prefix read about ten blocks whatever the prefix's length:

```sql
CREATE TABLE queries (id KEYWORD PRIMARY KEY, n INT) OPTIONS(cluster_by = 'id');
```

Over ORCAS's 10.4 million queries, loaded in no particular order and merged by the default policy (11 segments), 2,000 typed queries drawn by clicks, one thread on a 4-core cloud VM (Intel Xeon @ 2.10GHz), p99 in ms:

| characters typed | 1 | 2 | 3 | 4 | 6 | 8 | 12 |
|---|---:|---:|---:|---:|---:|---:|---:|
| rows matching (median) | 560,354 | 64,887 | 9,461 | 2,418 | 415 | 68 | 6 |
| every match read | 46.7 | 25.4 | 18.2 | 17.1 | 9.9 | 9.1 | 2.6 |
| blocks best first | 9.5 | 13.0 | 8.8 | 9.7 | 6.3 | 6.5 | 2.6 |
| blocks, clustered by `id` | 0.27 | 0.53 | 0.48 | 0.38 | 0.35 | 0.33 | 0.28 |

The full runs, with medians and the 105 segments straight after the load, are [`results/text/complete-orcas.json` and `complete-orcas-clustered.json`](https://github.com/chloetech-ai/chloedb/tree/main/chloedb/results/text) (`chloedb-complete-bench`).

Typos cost the walk of each segment's dictionary, which finds each value's fewest edits. On the same clustered table, the median top 10 with `fuzzy => 'auto'` takes 1.9 ms at 3 letters and 2.7 ms at 4, where it allows one edit, and 8.2 to 11.9 ms from 6 letters, where it allows two (p99 up to 24 ms); with `fuzzy => 1` it takes 3.3 to 4.1 ms from 6 letters (p99 up to 8.5 ms). Up to 2 letters `'auto'` allows none, so it costs what an exact prefix does. Straight after a load, with 105 segments, the same queries take 2.7 to 3.9 times as long. Ranked exact completions first (the `CASE` above), the median is 5.1 to 15 ms from 2 letters and 28 ms at 1 (p99 up to 62 ms), since a ranking expression is not a column whose blocks can be read best first: every completion is ranked. Results: `complete-orcas-clustered-fuzzy.json`, `complete-orcas-clustered-fuzzy1.json` and `complete-orcas-clustered-fuzzy-exact-first.json` (one thread of the 4-core cloud VM, Intel Xeon @ 2.10GHz).

## Substrings: `LIKE` and `ILIKE` anywhere in a value

A `TEXT` or `KEYWORD` column declared with `OPTIONS(substring = true)` gets a substring index, and `LIKE` and `ILIKE` on it are answered from that index, whatever the pattern. It finds what `MATCH` cannot, because it has no tokenizer: part of a word, an IP address, a path, an identifier, punctuation, text in a script the analyzer does not split.

```sql
CREATE TABLE logs (
  id INT PRIMARY KEY,
  ts TIMESTAMP NOT NULL,
  host KEYWORD OPTIONS(substring = true),
  body TEXT OPTIONS(substring = true)
);

SELECT ts, body FROM logs WHERE body LIKE '%173.234.31.186%' ORDER BY ts DESC LIMIT 20;
SELECT count(*) FROM logs WHERE body ILIKE '%break-in attempt%' AND ts >= '2026-11-01';
SELECT id FROM logs WHERE body LIKE '%Failed password%from 119.4.203.64%' AND host NOT LIKE '%.internal';
```

- **The answer is the one a test of every row gives.** `%` is any run of characters and `_` one character; `LIKE` compares byte for byte, and `ILIKE` compares both sides lowercased a character at a time, as PostgreSQL does and as it does [over joined or derived rows](#subqueries-with-and-joins): a capital `Σ` matches `σ` and the final `ς` wherever it stands, and a capital `İ` is one `i` <!-- since 0.3.4 -->. A row without a value matches neither a pattern nor its `NOT`, and a NULL pattern (`LIKE :p` with `:p` NULL) matches no row, nor does its `NOT` <!-- since 0.3.4 -->. The index only changes how long the answer takes.
- **How it answers.** Each segment holds an FM-index of the field's values: the Burrows-Wheeler transform of their bytes in a wavelet tree, with a sample of the suffix array that says which row a match is in. Counting the places a string occurs takes time by the string's length, not by the table's size, and each row found then costs a microsecond or two. A pattern with one string (`'%lit%'`, `'lit%'`, `'%lit'`, `'lit'`) is answered from the index alone. A pattern with several (`'%a%b%'`) or with `_`, and every `ILIKE`, takes its candidates from the string that occurs least and tests their stored values; `ILIKE` searches the spellings that exist (`error`, `Error`, `ERROR`), not every combination of cases.
- **A pattern in too many rows reads the values instead.** Where a string occurs in more than one row for every KiB of a segment's text (for every 4 KiB under a `LIKE` whose candidates' values are tested too), listing its rows from the index would cost more than testing each value, so the query tests the values, as a table without the index does. Beside conditions that leave few rows (`pid = 24200 AND body LIKE '%e%'`), the pattern is tested on those rows' values alone, whichever of the two the index's counts say is cheaper.
- **What the column must be**: one value per row and stored (the stored values answer for rows not yet in a segment's index, and for the candidates above). It need not be searched by `MATCH`: `body TEXT OPTIONS(substring = true, indexed = false)` keeps only the substring index. `ALTER TABLE ... ADD COLUMN` takes the option. In JSON it is `"substring": true`.
- **A `KEYWORD` column's value or prefix** (`LIKE 'abc'`, `LIKE 'abc%'`) still reads its dictionary, which is faster, and takes `ESCAPE`. A pattern the substring index answers does not take `ESCAPE`.
- **Rows just written** are tested by their values until a flush writes their segment; a segment gets its index when it is flushed or merged.
- **What it costs.** On three logs of 0.7 to 4.7 million lines the index took 0.94 to 1.02 of the bytes of the text it indexes, which made the file 1.8 to 2.3 times as large; loading took 1.3 to 2.6 times as long and a merge 2.2 to 3.6 times, since each segment's index is built when it is flushed and again when it is merged. A pattern in few rows took 0.02 to 2 ms where a test of every row took 0.6 to 4 s, and a pattern in very many rows a sixth to an eighth of that test ([`results/substring`](https://github.com/chloetech-ai/chloedb/tree/main/chloedb/results/substring), on a 4-core cloud VM, Intel Xeon @ 2.10GHz).
- It makes the database [file format 3](index.md#wal-memtable-and-snapshots) with the feature `substring:1`, which a version of ChloeDB from before it refuses by name. The format is specified in [`chloedb/notes/substring.md`](https://github.com/chloetech-ai/chloedb/blob/main/chloedb/notes/substring.md).

## Scores

### `bm25(field, text)`

BM25 of the analysed terms of `text`, disjunctively: a row scores if it contains any of them, unless the text [requires or excludes words](#weights-required-and-excluded-words). Ranked queries drop English function words (NLTK's 153-word list), except that its single letters (`d`, `m`, `o`, `s`, `t`, `y`) are kept unless they end a contraction: "vitamin d" and "T cell" keep their letter, "don't" and "it's" drop it. A term written twice weighs twice. Used in `ORDER BY` without a `MATCH`, it is a ranked search; with one, it ranks the matching rows.

| option | default | effect |
|---|---|---|
| `k1`, `b` | 1.2, 0.75 | BM25 parameters (`k1 >= 0`, `0 <= b <= 1`) |
| `algo` | planner's choice (MaxScore) | `'maxscore'`, `'bmw'` (Block-Max WAND) or `'exhaustive'` |
| `stopwords` | 1 (0 on a `simple` field) | `0` keeps English function words in the query, `1` drops them |
| `fuzzy` | 0 | `1`, `2` or `'auto'`: also score the indexed terms that many edits from each word; `'auto'` corrects only the words that look like typos ([typos](#typos-and-synonyms)) |
| `synonyms` | 0 | `1`: also score each word's synonyms |
| `syntax` | `'query'` | `'plain'` reads `+`, `-`, quotes and `^` as punctuation: the text is words alone; `'terms'` reads analysed terms, each weighted by `^w`, as they are written ([query expansion](#query-expansion-expand)) |
| `min` | none | `n` or `'p%'`: rank only the rows with at least that many of the text's optional words and phrases |
| `expand` | 0 | `n`: also rank by the terms the query's top `n` rows hold most ([query expansion](#query-expansion-expand)), with `expand_terms` (20) and `expand_weight` (0.85) |

Keeping function words is better for short questions: on Quora it gains 0.055 nDCG@10, while on TREC-COVID, whose queries are full questions, dropping them gains the same amount.

#### Weights, required and excluded words

The text may weigh a word, require it or exclude it, as a search box does:

| text | ranks |
|---|---|
| `vaccine mrna` | the rows with either word |
| `vaccine^2 mrna` | the same rows, `vaccine` weighing twice: as `vaccine vaccine mrna` |
| `+mrna vaccine` | the rows with `mrna`, by both words |
| `vaccine -animal` | the rows with `vaccine` and without `animal` |
| `+"messenger rna" vaccine^0.5` | the rows with the phrase (with both words, on a field without positions), by its words and half of `vaccine` |
| `+covid^0 treatment` | the rows with `covid`, by `treatment` alone |

- **`^w`** after a word or a closing quote multiplies its terms' BM25 by `w`, a number of at least 0 (`2`, `0.5`, `.5`). Weights add up, as a word written twice does. A word weighing 0 scores nothing, and a text whose words all weigh 0 ranks no row.
- **`+`** before a word or a quoted phrase requires it, **`-`** excludes it: a row must contain every required one and none of the excluded ones, and ranks by the others too. An excluded word does not score, and a word the query drops as a stopword is neither required nor excluded. A sign counts only at the start of a word and before a letter, digit or quote: `covid-19`, a `-` alone, `+-x` and `x^` are words. Quotes alone change nothing: `"messenger rna"` scores what `messenger rna` does.
- With `fuzzy` or `synonyms`, a word's typo neighbours and synonyms take the highest weight the word is written with (writing it twice weighs the word twice, not them), and a required or excluded word counts as present when one of them is, as in `MATCH`.
- A required or excluded word is a filter on the ranked rows: the top-k loop (all three `algo`s), a [rank-safe boost](#rank-safe-boosts), a [rerank phase](#rerank-phases) (where a row the filter drops scores 0, as one without the words does) and shards all apply it, so the rows and scores are those of the rows the text allows.
- Weights and signs cost little. With each query's first word weighed 2 (`word^2`) or required (`+word`), or its last word excluded (`-word`), the median query took 0.27, 0.28 and 0.23 ms on Quora's 10,000 queries against 0.25 ms for the plain text, and 0.24, 0.18 and 0.22 ms on FiQA against 0.22 ms; a required word can make a query faster, as the filter drops rows before they are scored. Text written without operators ranks and times as the plain text does on all five collections measured (`chloedb/results/text/bm25_term_operators.json`, one core of a 4-core cloud VM while another job held the other three). The operators were put on words mechanically to time them, so these queries rank worse than the plain text: an operator helps when it says what the person searching means.
- **`min => n`** (or `'p%'`) ranks only the rows with at least `n` of the text's optional words and phrases: those without a sign, each counted once however often it is written, whatever its weight (`^0` counts too). Required and excluded words are not counted; a text without optional words ignores `min`. Counts are clamped to between 1 and the number of optional words, and a percentage rounds down, as in `MATCH`'s groups: `bm25(body, 'vaccine mrna trial phase', min => 2)` ranks the rows with two of the four words, by all of them. With `syntax => 'plain'`, each word is an optional word.
- `min` filters the ranked rows, as a required word does, so its cost depends on how many rows hold one of the words. With `min => 2` the median query took 0.24 ms on Quora's 10,000 queries (0.25 ms plain) and 0.41 ms on FiQA (0.23 ms plain). On TREC-COVID, whose query words appear in most of its documents, it took 1.6 ms (0.5 ms plain), because the filter counts every row that holds one of the words (`chloedb/results/text/bm25_min_match.json`, one core of a 4-core cloud VM while another job held the other three). `min` is for leaving rows out, not for ranking better. Asking every BEIR query for two of its words, or half of them, left nDCG@10 unchanged on TREC-COVID and lowered it elsewhere, by as much as 0.077 (SciFact, half the words).
- **`syntax => 'plain'`** reads the text as words alone, as `bm25` did before: for text not written for a search box, such as a user's message passed through, or a benchmark's recorded queries. BEIR's hold `+1`, `-word` or `x^2` here and there, so the evaluations and benchmarks in this repository pass `syntax => 'plain'`, and so does ChloeCtx.
- A plan that ranks with weights, signs, or a `MATCH` with weights needs the plan feature `text_operators`, and one with `min` or a `MATCH` group `(…)@n` needs `min_match`. From Python, `bm25("body", "+mrna vaccine^2")` reads them, `bm25("body", text, syntax="plain")` does not, and `bm25("body", text, min=2)` and `match("body", text, min="75%")` ask for some of the words.

**Changed:** `bm25` read `+`, `-`, `"` and `^` as punctuation before; text with them now ranks as above, and `syntax => 'plain'` keeps the old reading. In `MATCH`, `^` separated words: `vaccine^2` matched `vaccine` and `2`, and now matches `vaccine`; and `)@2` closes a group a row needs 2 of, where `@2` was a word before.

### `similarity(field, vector)` and `distance(field, vector)`

The dot product (cosine for unit vectors) and the Euclidean (L2) distance between a row's vector and the query vector, which is a bound parameter or a JSON array of the field's dimension.

**Text instead of a vector.** On a field a model embeds (`OPTIONS(embed = 'body', model = '...')`), the second argument may be text, a literal or a parameter that holds text: `ORDER BY similarity(emb, 'how do I reset my password') DESC`, or `similarity(emb, :q)` with `:q` bound to a string. The text is embedded as a query by the field's own model when the query is planned, once however often the query repeats it, and the query runs as if its vector had been given (`distance` and `maxsim` take text the same way; on a multi-vector field the text is its tokens' vectors). `similarity(emb, embed(:q))` says the same. In a [rank profile](#rank-profiles-a-ranking-kept-with-its-table) too, on chloedb-server: one text parameter feeds both `bm25(title, :q)` and `similarity(emb, :q)` (the profile written out, its texts embedded, the plan still naming the profile). A string that is a JSON array of numbers is still a vector, and a `SPARSE` field's terms are still text.

**`embed('model', text)`** is the query vector of a text by the model named, wherever a vector goes: against a field of no model, `similarity(raw, embed('potion-base-8M', :q))`, or in a projection, `SELECT id, embed('potion-base-8M', 'tents') AS v FROM docs`. The model is one an embedded field uses, `hash:N`, or a directory directly under the models directory, named as `llm()` names one (letters, digits, `_`, `-` and `.`, never a path; `chloedb-server fetch-model` fetches the known ones). A model behind an API (`openai/...`, an `endpoint.json`) is taken only when a field uses it, so that a query spends no key a table was not given. The text is a value or a parameter, not a column: the model runs once a query, not once a row, and a column is embedded when rows are written, by declaring its vector field `OPTIONS(embed = 'column', model = '...')`.

The text is embedded where the query is planned: by the database from Rust, Python and JavaScript, and by the server (a coordinator for its shards) on the cores it embeds writes with, as a parameter's `{"embed": "text"}` is. With a static model (Model2Vec, such as `potion-retrieval-32M`) that takes tens of microseconds; the median query on SciFact, its text embedded and every row measured, took 0.45 ms ([results](../../chloedb/results/static-int8/README.md)).

`similarity` is the raw dot product of the vectors as written: ChloeDB does not normalise the vectors a write supplies (the models it runs itself write unit vectors), so on vectors of other lengths long vectors rank first. For cosine similarity, write unit vectors. `ORDER BY distance(emb, :v) ASC` ranks by $\lVert v - q \rVert$, nearest first, and `score()` is minus the distance (ranking is always best first). On a field a model embeds the vectors are unit length, where distance orders rows as the dot product does, so the index searches them (`strategy => 'flat'` measures every row); on any other field every row `WHERE` keeps is measured exactly, a scan of the field's float32 vectors, since its index ranks by the dot product. **Changed:** `distance()` ranked like `similarity(...) DESC` before, which is the nearest row only for unit vectors, and its `score()` was the similarity; a shard from before this change still ranks it so. In an expression, `distance()` stays refused: use `similarity()`.

| option | effect |
|---|---|
| `strategy` | `'auto'` (default: the planner decides, by its cost model, which sizes the work by the vectors' dimensions: an unfiltered search scans every vector exactly where that costs no more than the index, as in a segment of 20,000 vectors of 64 dimensions), `'prefilter'` (evaluate the filter, scan its members exactly), `'index'` (search HNSW or IVF with the filter inside), `'postfilter'` (search with a larger *k* and drop non-matching rows), `'flat'` (exact scan), `'partition'` (walk the graphs of the [partitions](#vector-partitions) the filter can match) |
| `ef` | HNSW search breadth (default 200; 64 before). On random unit vectors of 64 dimensions, the hard case for a graph (real embeddings cluster, and need less), the index found 0.73 of the true top 10 at ef 64 and 0.96 at 200 over 20,000 rows (0.13 and 0.24 ms at the median), and 0.40 and 0.71 over 200,000 (0.24 and 0.35 ms); the default search of the 20,000, now an exact scan, finds all of them in 0.15 ms (200 queries, `cargo test --test vector_defaults -- --ignored`, one run on a 4-core Intel Xeon @ 2.10GHz shared with other builds) |
| `nprobe` | IVF clusters probed |
| `rerank` | how many candidates are re-scored in float32 |

#### Vector partitions

A filter that keeps a fifth of the rows makes an HNSW search either walk its graph testing every row it reaches, or fetch five times the candidates and drop the rest. A field partitioned by the column the filter names does neither: each segment keeps, besides the field's graph of every row, a graph per partition of the rows, and a search whose `WHERE` limits that column walks only the graphs of the partitions that can match.

```sql
CREATE TABLE docs (id KEYWORD PRIMARY KEY, rating FLOAT, category KEYWORD,
  emb VECTOR(768) OPTIONS(partition_by = 'rating', partition_bounds = '1, 2, 3, 4'));

SELECT id FROM docs WHERE rating >= 4 ORDER BY similarity(emb, :v) DESC LIMIT 10;
```

- `partition_bounds` splits an `INT` or `FLOAT` column into ranges: here below 1, 1 to 2, 2 to 3, 3 to 4, and 4 and up (1 to 63 increasing numbers). A `KEYWORD` column is partitioned by value and takes no bounds (`partition_by = 'category'`): each segment builds a graph for each of its 64 most frequent values. The column must be stored, indexed and single-valued, and the field an HNSW field of one vector a row. Rows without a value in the column are only in the field's graph.
- The planner reads the first condition on the column among the filter's top-level `AND`s: a range (`>=`, `<`, `BETWEEN`), `=` or `IN`. A partition whose rows all pass it is walked with no test at all, so `rating >= 4` costs what an unfiltered search of a fifth of the rows costs; a partition that only partly matches, or a filter with other conditions, tests the rest on each row the walk reaches. The planner uses partitions when its cost model predicts them faster than its other plans, and `strategy => 'partition'` uses them wherever they apply. A search without a condition on the column uses the field's graph, as before.
- On 1M MS MARCO passages (768-dimension vectors, one segment, the 4-core cloud container), the engine comparison's `rating >= 4` (20% of rows) took 176–184 µs with partitions against 364–438 µs, at recall@10 0.963 against 0.961; `rating >= 4.5` (10%) 383 µs against 674–777 µs. At 2% (`rating >= 4.9`) the partitions took 1.1 ms where the planner's exact scan of the matching rows takes 1.6–2.2 ms, finding 0.995 of the exact answers instead of 0.999 (`chloedb/results/vector/partitions-msmarco-1000000x768.json`).
- The partitions' graphs take as much space as the field's graph again (128 bytes a row at `m = 16`, 3% of that table's file) but share its vectors and codes. Building that table took 320 s instead of 179 s, and merges build the partitions again from scratch.
- A database with partitions is written in [file format 3](index.md#wal-memtable-and-snapshots) with the feature `vector_partition`, which versions of ChloeDB from before them refuse by name.

A `VECTOR` field keeps, besides its float32 rows, a compact code per row that the search scans: 8 bits per dimension by default, or 1 bit per dimension with `"codes": "binary"` in the field's schema (or the build option `vector.codes = 'binary'`). Binary codes are 32 times smaller than the vectors and 8 times smaller than 8-bit codes; to make up for the coarser scores, the search re-scores 8 times as many candidates in float32. On synthetic 768-dimensional data that kept recall@10 at 0.985–0.994 with flat, IVF and HNSW search, at an eighth of the memory ([results](results.md#compressed-vectors)); check recall on your own embeddings before choosing them, since some models lose more to one-bit codes than others.

RaBitQ codes (`"codes": "rabitq1"`, `"rabitq2"` or `"rabitq4"`, or `codes = 'rabitq4'` in `OPTIONS`) keep 1, 2 or 4 bits per dimension of the vector after a fixed random rotation, and two numbers per row that give each estimated score an error bound. The search keeps 8, 4 or 2 times as many candidates, and skips the float32 row of any candidate whose bound shows it cannot reach the top results found so far. On FiQA's Cohere embeddings, at recall@10 1.000 with a flat scan, 1-bit RaBitQ codes read 91 float32 rows a query where binary codes read 320, 2-bit codes 28 and 4-bit codes 13, against SQ8's 40 ([results](results.md#compressed-vectors)). Their scan is slower per row than binary or SQ8 codes of the same field: 4-bit codes take half the memory of SQ8 and scan about 1.5 times slower. They pay off when reading the float32 rows is the cost, as with a table larger than memory. A database with RaBitQ codes is written in [file format 3](index.md#wal-memtable-and-snapshots) with the feature `rabitq`, which versions of ChloeDB from before them refuse by name.

### `similarity(field, weights)` on a `SPARSE` field

The dot product of a row's term weights with the query's, `Σ q_t · w_t` over the terms both have. The query is a JSON object of term weights, bound as a parameter or written as a string:

```sql
SELECT id, score() FROM docs
ORDER BY similarity(splade, '{"vaccine": 1.4, "mrna": 2.1, "efficacy": 0.6}') DESC LIMIT 10;
```

It runs on the same machinery as `bm25` (MaxScore, Block-Max WAND, the filter strategies), so `algo => 'maxscore' | 'bmw' | 'exhaustive'` applies, and it fuses with other scores in `rrf`, weighted sums and products. `distance` is not defined on a `SPARSE` field.

A `SPARSE` field [embedded by a learned sparse model](server.md#term-weights-of-a-learned-sparse-model) (`sp SPARSE OPTIONS(embed = 'body', model = '...')`) gets its rows' weights from the model; the query's are the parameter `{"embed": "some words", "field": "sp"}` (from Rust, `Db::embed_query_in`), weighed by the same model.

### `maxsim(field, vectors)`: several query vectors

The best similarity of a row's vector with any of several query vectors, $\max_i q_i \cdot v$: a row near any one of a user's recent items, or of a query's several phrasings. The vectors are a bound list of vectors, a tuple of vector parameters, a JSON array of arrays, or a subquery that returns one column of vectors, which runs first:

```sql
-- items like any of the user's last 20, by similarity
SELECT id, score() FROM items
ORDER BY maxsim(emb, (SELECT emb FROM history WHERE usr = :u ORDER BY ts DESC LIMIT 20)) DESC LIMIT 10;

-- and fused with text and a row's rating
SELECT id FROM items
ORDER BY rrf(bm25(title, :t), maxsim(emb, :recent)) * (1 + 0.1 * coalesce(rating, 0)) DESC LIMIT 10;
```

It runs one search per query vector, with the filter and the options of `similarity` (`strategy`, `ef`, `nprobe`, `rerank`), and scores the union of their top *k* exactly against every query vector. The union holds the exact top *k*: a row among them is among the top *k* of the query vector it is closest to. Each added query vector costs one more search ([what that costs](results.md#maxsim)). A row without a vector scores `NULL`, a subquery with no rows ranks nothing, and more than 64 vectors is an error unless `max_vectors => n` raises the limit. It is a score wherever `similarity` is one: in `rrf`, `minmax()` sums and ranking expressions, on one database or over shards.

On a **multi-vector field** (`VECTOR(d)[]`), `maxsim` is late interaction as ColBERT and ColPali score it: the sum over the query vectors of each one's best match among the row's vectors, $\sum_i \max_j q_i \cdot v_j$. Each query vector's search returns its best `depth` vectors (64), which name candidate rows; the `candidates` rows (200, at least the `LIMIT`) with the largest sum of the matches found are scored over all their vectors, with their 8-bit codes, or float32 when the field keeps them. A row without vectors ranks nowhere. `depth => n` and `candidates => n` trade time for recall; with both at least the number of vectors, the answer is exact. `similarity` refuses a multi-vector field, and so do graph functions.

```sql
-- pages by their patches' best matches with the query's token vectors
SELECT id, score() FROM pages ORDER BY maxsim(patches, :tokens) DESC LIMIT 5;
```

A multi-vector field can also be the vectors of a text's tokens, which a model of late interaction (ColBERT) computes when rows are written: `tok VECTOR(96)[] OPTIONS(embed = 'body', model = 'answerai-colbert-small-v1')` ([a vector a token](server.md#a-vector-a-token)). A query gives the vectors of its own tokens as the parameter (`{"embed": "the text", "field": "tok"}` on the server, `Db::embed_query_in` from Rust). Its usual place is a rerank phase over BM25's best rows, which scores each of them over all its vectors:

```sql
WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :q) DESC LIMIT 100)
SELECT id, score() FROM top ORDER BY maxsim(tok, :tokens) DESC LIMIT 10;
```

On BEIR's SciFact and NFCorpus, `answerai-colbert-small-v1` over BM25's top 20 scores nDCG@10 0.738 and 0.354 (BM25 alone 0.691 and 0.329) in 80–90 ms a query on four cores, most of it encoding the query; that ranks better than the cross-encoder below, in a fifth of its time. The cost is at write: 0.1–0.16 s a document to embed, and a database about 29 times BM25's (half that with `pool_factor = 2`, which scores the same). The [results](results.md#colbert-reranking) have the details.

`dot(a, b)` is the dot product of two vectors as a value, for rows in memory: `SELECT id, dot(emb, :q) FROM h` over a `WITH` result, a join, or a table's own rows. It equals `similarity` to the bit, and in a ranking expression over a table (`ORDER BY bm25(title, :q) + 2 * coalesce(dot(emb, :q), 0) DESC`) it is `similarity(emb, :q)`: `NULL` for a row without a vector.

### `rrf(s1, s2, …)`: reciprocal rank fusion

Each input ranks the rows that pass `WHERE`, to a depth of 100 by default, and a row's fused score is

$$
\mathrm{rrf}(d) = \sum_{i} \frac{1}{k + \mathrm{rank}_i(d)}, \qquad k = 60
$$

Options: `k => 60`, `depth => 100`. The depth is raised to the page's `LIMIT` when it asks for more rows, as in weighted sums and products, but not by its `OFFSET`: pages of one size (`LIMIT 20 OFFSET 0, 20, 40 ...`) rank the same candidates, so they are the pages of one ranking, with no row on two pages and none skipped. A page past the candidates comes back short, with `truncated: true` and a plan line that says so; to page deeper, give the ranking a larger `depth =>`, the same on every page. **Changed:** the depth was raised to `LIMIT + OFFSET` before, so each page ranked other candidates, and paging repeated and skipped rows.

### `ranked(keys)`: a ranking from elsewhere

`ranked(:keys)` is a ranking computed outside ChloeDB, given as primary keys, best first: a recommender's candidates, a reranker's order, or the rows a [semantic-ID](server.md#semantic-ids) decoder generated. It is a score like any other, so it fuses with ChloeDB's own rankings:

```sql
SELECT id FROM items WHERE in_stock ORDER BY rrf(bm25(title, :q), ranked(:keys)) DESC LIMIT 20;
```

A listed row scores `-(its position)`, from `-1` for the first key; a key listed twice ranks where it is first. A row the list leaves out has `NULL`, so in an expression it ranks last unless `coalesce` gives it a value, and in a weighted sum it takes the input's worst value, as a missing column does. Times another score (`ranked(:keys) * rating`) it multiplies as arithmetic, not as a product of scores, which would count its negative values as 0. A key no live row has is skipped. On its own, `ORDER BY ranked(:keys) DESC` answers the listed rows that `WHERE` keeps, in list order. The keys are a parameter holding a list (an array of keys in the HTTP API), a tuple (`ranked(('a', 'b'))`) or an array (`ranked(ARRAY['a', 'b'])`). Over shards, each shard scores its own rows from the list and the coordinator merges them, so no extra round is needed. A plan that uses it needs the plan feature `ranked`.

### Weighted sums: `tmm()`, `minmax()` and `zscore()`

A weighted sum of normalised scores is weighted (convex) fusion: `0.2 * tmm(bm25(body, :t)) + 0.8 * tmm(similarity(emb, :q))`. Weights are numeric constants; `+` and `-` combine any number of terms. Candidates are the union of each input's top 100, re-scored exactly by every input and normalised over that union ($C$). Three normalisers put a score on one scale:

| normaliser | a row's value | a row without a value |
|---|---|---|
| `tmm(x)`, theoretical min-max | $\dfrac{x - lo}{\max_C x - lo}$, `lo` the least value `x` can take | 0 |
| `minmax(x)` | $\dfrac{x - \min_C x}{\max_C x - \min_C x}$, 1 where all are equal | 0 |
| `zscore(x)` | $\dfrac{x - \mathrm{mean}_C x}{\mathrm{sd}_C x}$, 0 where all are equal | the least of the candidates' |

Use `tmm` unless your own judged queries say otherwise. Its lower end is fixed by what the score is, not by the candidates: a row's value depends only on the best candidate, which is the same at any depth, while `minmax` moves every row whenever the weakest candidate changes, which it does with `depth` and with what each input happens to return. Bruch, Gai and Ingber ([TOIS 2023](https://arxiv.org/abs/2210.11934)) find this convex combination beats RRF in and out of domain once its one weight is tuned, and that a few dozen judged queries tune it; the [relevance lab](server.md) tunes it on yours ([what it gives on BEIR](results.md#score-fusion)).

`lo` is the score's own: 0 for `bm25`, a `SPARSE` similarity (weights at least 0, as learned models give them), `rrf`, `ppr` and `decay`; -1 for `similarity` and a one-vector `maxsim`, the least cosine of unit vectors (the vectors of an embedded field, and of most models; for vectors of other lengths, give `lo`); -n for `maxsim` of n vectors on a multi-vector field. Anything else, a column or an expression, needs it written: `tmm(rating, lo => 0)`. With `hi` too, `tmm(x, lo => a, hi => b)` is $(x - a) / (b - a)$, each row's own value (0 without one), fixed bounds that read nothing of the other candidates.

Options, given on any input: `depth => 100` (candidates per input), `exact => 1` (score every row that passes `WHERE` instead of the top-100 union). The top-100 shortcut recovers 89–95% of the exact fused top 10 ([results](results.md#hybrid-ranking)).

A row without a value of a column input (`0.7 * minmax(bm25(body, :t)) + 0.3 * minmax(rating)` for a row whose `rating` is `NULL`) takes that input's worst value among the candidates: its lowest when the weight is positive and its highest when the weight is negative (0 and 1 after min-max), so a missing value never lifts a row. Under `tmm` and `zscore` it takes the value in the table above. A plan with `tmm` or `zscore` needs the plan feature `score_norms`, which a version of ChloeDB from before them refuses by name.

**Changed:** a sum of bare scores, `0.2 * bm25(body, :t) + 0.8 * similarity(emb, :q)`, was this fusion before ranking expressions; `+` now adds raw values, as in any expression below. Write `minmax()` around each term for the fusion. For one release, `EXPLAIN` and a query's plan lines add a note when an expression adds raw relevance scores. `norm => 'none'`, which kept raw scores in a sum, is accepted and changes nothing.

### Ranking expressions

`ORDER BY` takes any expression of scores, numeric columns and numbers, ranked best first (`DESC`; negate it to rank the smallest first):

```sql
-- relevance boosted by a rating, which may be missing
SELECT id FROM products ORDER BY ln(1 + bm25(body, :t)) * (1 + 0.2 * coalesce(rating, 0)) DESC LIMIT 10;

-- a business rule on top of similarity
SELECT id FROM products
ORDER BY similarity(emb, :q) + CASE WHEN in_stock THEN 0.1 WHEN category = 'outlet' THEN -0.2 ELSE 0 END DESC LIMIT 10;

-- Borda-style fusion of two rankings
SELECT id FROM papers ORDER BY -rank(bm25(body, :t)) - rank(similarity(emb, :q)) DESC LIMIT 10;
```

| part | meaning |
|---|---|
| `+ - * / %`, unary `-` | arithmetic on raw values |
| `bm25`, `similarity`, `ppr`, `decay`, `rrf`, `ranked`, a numeric column, `hops()`, `seq_no()` | the leaves: a row's value of each |
| `ln(x)`, `log(x)` (base 10), `log(b, x)`, `exp(x)`, `sqrt(x)`, `pow(x, y)`, `abs(x)`, `round(x[, digits])`, `sigmoid(x)`, `clamp(x, lo, hi)` | numeric functions |
| `least(…)`, `greatest(…)`, `coalesce(…)` | over their arguments that are not `NULL` |
| `tmm(x[, lo => a][, hi => b])` | `x` from its least possible value to the best row's, put on [0, 1]: 0 where `x` is `NULL` ([above](#weighted-sums-tmm-minmax-and-zscore)) |
| `minmax(x)` | `x` over the rows ranked, put on [0, 1]: 1 where all are equal, 0 where `x` is `NULL` |
| `zscore(x)` | `x` less the rows' mean, over their deviation; `NULL` takes the least of the rows' |
| `rank(x)` | 1 plus the number of rows ranked with a greater `x`; `NULL` places after every other |
| `CASE WHEN c THEN x … ELSE y END` | the first branch whose condition holds (any `WHERE` condition), else `y`, else `NULL` |
| `count_true(c, …)` | how many of the conditions hold ([`AT_LEAST`](#some-of-the-conditions-at_least)) |

`NULL` is a missing value (a `NULL` column, a row without a vector, one a traversal does not reach) or a result that is not a finite number (`ln(0)`, a division by 0, an overflow). Arithmetic and functions of `NULL` are `NULL`, except `coalesce`, `least` and `greatest`, which skip it. A row whose score is `NULL` ranks after every other, and its `score()` is `NULL`.

Candidates are the top `depth` (100) rows of each relevance leaf (`bm25`, `similarity`, a graph's, `ranked`), and of every input of `rrf`; an expression with no such leaf (`rating * 2 + year / 1000`), or `exact => 1` on any leaf, ranks every row that passes `WHERE`. A [rank-safe boost](#rank-safe-boosts) ranks every row its text score matches. Each leaf scores every candidate exactly, and `tmm()`, `minmax()`, `zscore()` and `rank()` are taken over the candidates. Over shards, each shard returns its candidates' leaf values and the coordinator runs the expression, so a split collection ranks like the whole one.

#### Rank-safe boosts

An expression with one text score (`bm25`, or `similarity` on a `SPARSE` field) whose other parts are numeric columns, `decay`, `CASE` conditions, numbers and `seq_no()` finds its exact top rows: the rows that scoring every row the text matches would rank first, not only the best of the text's top 100. Set functions (`tmm()`, `minmax()`, `zscore()`, `rank()`), `rrf` and `exact => 1` take the candidates above instead.

```sql
-- the 10 best by BM25 boosted by importance, even a row 5,000th by BM25 alone
SELECT id FROM docs ORDER BY bm25(body, :t) * (1 + 0.2 * importance) DESC LIMIT 10;
```

The text score's top-k loop (MaxScore, Block-Max WAND or exhaustive, as `algo` says) ranks by the whole expression. From the k-th best value so far and each column's smallest and largest value, it works out the lowest text score a row needs, and skips the rows below it. A row the text does not match is not a candidate. When the expression does not rise with the text score (`rating - bm25(body, :t)`), no text score can be ruled out: the loop scores every matching row, and the plan line says `every match scored`. Over shards, each shard returns its own exact top rows: its top `LIMIT + OFFSET` over all its segments, and the rows tied with the last of them. The cluster's top rows are each in their own shard's, so a split collection ranks like the whole one. A shard also returns the text score's top `depth` rows, which a coordinator from before rank-safe boosts fused from, only to a coordinator older than shard protocol 10 ([gRPC](server.md#grpc)).

A plan that ranks by an expression needs the plan feature `rank_expressions`; a version of ChloeDB from before it refuses the plan by that name. Weighted sums of `minmax()`, products of scores and `rrf` are written as before, so older shards still run them.

#### Rerank phases

A ranking can go on in phases, as a search engine's rank profile does: a cheap first phase over every candidate, then a costlier one over its top rows. SQL writes a phase as a `WITH` that ranks a table and cuts it, ranked again by the next `WITH` or the query itself:

```sql
-- BM25's top 100, ranked again by a mix of title and body relevance and rating
WITH top AS (SELECT * FROM docs WHERE year >= 2020 ORDER BY bm25(body, :t) DESC LIMIT 100)
SELECT id, title, score() FROM top
ORDER BY bm25(title, :t) + 0.5 * bm25(body, :t) + 0.1 * coalesce(rating, 0) DESC LIMIT 10;

-- three phases: similarity's top 500, its best 50 by a rating, then a fusion of the 50
WITH a AS (SELECT * FROM products ORDER BY similarity(emb, :q) DESC LIMIT 500),
     b AS (SELECT * FROM a ORDER BY coalesce(rating, 0) DESC LIMIT 50)
SELECT id, score() FROM b ORDER BY minmax(bm25(body, :t)) + minmax(similarity(emb, :q)) DESC LIMIT 10;
```

The planner makes one search of them, with phases. Each phase ranks the rows of the one before by an expression, best first, and may read any of the table's columns, since the rows are the table's. Every row a phase reads goes on: one the expression gives no value ranks last with a `NULL` score (and a row a `bm25` phase's terms miss scores 0), and rows tied keep the order the phase before gave them. `LIMIT` and `OFFSET` on the query cut the last phase, and `score()` is the last phase's score. The first `WITH` may filter (`WHERE`) and rank as any query does; each later step reads the step before alone, with no `WHERE` or grouping, ranks by one expression `DESC` (which may hold [correlated subqueries](#correlated-subqueries-a-ranking-that-reads-other-rows)), and each `WITH` keeps the table's columns (`*`) and has a `LIMIT`. `WITH` results before the first phase, a user's history say, are computed first, for the phases' subqueries to read. Other `WITH` queries plan as before: the table restricted to the rows of the `WITH`.

A phase whose expression is row-local is a second phase: over shards, each shard scores it on its own top rows by the first phase. A phase with a set function (`tmm()`, `minmax()`, `zscore()`, `rank()`), `rrf` or a weighted sum reads every row it reranks, so it is a global phase: it runs once over the merged rows (on the coordinator over shards), and so does every phase after it. The coordinator keeps the top rows of the whole collection by the first phase before it ranks them by the next, so a split collection answers exactly as the whole one does. `EXPLAIN` shows each phase and which kind it is, and a query's plan lines say how many rows each reranked. The first phase's `LIMIT` counts against the rows a query may reach (`max_rows`), and the phases against its deadline.

From Python, `rerank(expr, count)` adds a phase: `t.select("id", score()).order_by(bm25("body", q)).rerank(col("rating"), 100).limit(10)`. A plan with phases needs the plan feature `rank_phases`.

#### Correlated subqueries: a ranking that reads other rows

A scalar subquery in a ranking may name a column of the row it ranks (`first.cat`). It is then correlated, and gives a number for each row: how often the user met the row's category, how long ago, how close the row is to what they bought.

```sql
-- the user's last 50 events with their items' categories and embeddings, then
-- BM25's top 1,000 reranked by what the user did with each one's category
WITH h AS (SELECT e.item AS item, i.cat AS cat, i.emb AS emb, e.action AS action, e.ts AS ts
           FROM events e JOIN items i ON e.item = i.id WHERE e.usr = :u ORDER BY e.ts DESC LIMIT 50),
     first AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT 1000)
SELECT id, title, score() FROM first
ORDER BY bm25(title, :q)
       + 0.5 * (SELECT COUNT(*) FROM h WHERE h.cat = first.cat)
       - 0.0001 * coalesce((SELECT MIN(:now - h.ts) FROM h WHERE h.cat = first.cat), 100000)
       + coalesce((SELECT MAX(dot(h.emb, first.emb)) FROM h WHERE h.action = 'buy'), 0) DESC
LIMIT 20;

-- a table read through its indexes: the user's events in the row's category that cost less
SELECT id FROM items WHERE MATCH(title, :q)
ORDER BY bm25(title, :q) + 0.1 * (SELECT COUNT(*) FROM events e WHERE e.usr = :u AND e.cat = items.cat AND e.price < items.price) DESC
LIMIT 20;

-- as a column of the rows ranked
WITH first AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT 100)
SELECT id, (SELECT COUNT(*) FROM events e WHERE e.usr = :u AND e.item = first.id) AS seen FROM first
ORDER BY bm25(title, :q) DESC LIMIT 20;
```

- **Where.** The rows ranked must be a bounded set: a [rerank phase](#rerank-phases), whose rows are the phase before's top rows; an `ORDER BY` over a table, or over a `WITH` result, of at most 10,000 rows (more is an error that suggests a phase); or the columns of a ranked query, alone or in an expression (`coalesce(price, 0) + (SELECT ...)`). A subquery naming the outer row in `WHERE`, `IN` or `EXISTS` is an error, but for an [`EXISTS` that names it by one equality](#subqueries-with-and-joins), a semi join.
- **What it names.** `name.column`, where `name` is the outer query's table or `WITH` result (or its alias) and the column one of its columns. The subquery reads a `WITH` result in memory, compared any way an expression over rows in memory can be, or a table through its indexes, whose conditions compare one of its columns with the outer row's (`e.cat = items.cat`, `e.price < items.price`) as they would a value.
- **Its value** is a number (`COUNT`, `SUM`, `MIN`, `MAX`, `AVG` of numbers, or a boolean as 0 or 1). One of no rows is `NULL` (`COUNT` gives 0), which ranks last unless `coalesce`d. A `NULL` of the outer row is compared as SQL compares it: `e.price < first.price` holds for no event when the row has no price.
- **How it runs.** First the rows ranked (the phases before, or the rows a bounded `ORDER BY` reads), with the columns the subqueries name. The parts of a subquery that do not read the outer row run once (`h` above, and its rows where `h.action = 'buy'`). A subquery whose outer columns meet only equalities ANDed into its `WHERE` (`h.cat = first.cat`) then runs once, grouped by them over the values the rows hold (`h.cat IN (...)`), and a value no group has takes the subquery's value over no rows; any other runs once for each distinct value of the columns it names. Then the ranking sorts the rows, with each row's values, and the plan lines say how much ran: `correlated: 3 subqueries over 1000 rows, 597 values, 3 runs`.
- **Over shards** a coordinator runs the steps before, and the subqueries over the shards' rows, and sends each row's values to the shards inside the plan, by key, so that every shard ranks with the same ones. A plan with them needs the plan feature `correlated`.
- **Cost** at 1,000 candidates, measured with `chloedb-correlated-bench` on a cloud VM (4-core Intel Xeon @ 2.10GHz, 16.9 GB, Linux 6.18): 100,000 items in 200 categories, 400,000 events of 2,000 users, a history of 50 events, one thread, 300 requests of each shape. BM25's top 1,000 reranked to 20 took 1.29 ms (mean) without subqueries, and reading the history added 1.67 ms. A subquery added, over the rerank alone (with the history's read):

  | subquery | values | runs | added, mean |
  |---|---|---|---|
  | by category, over `h` | 199 | 2 | 4.90 ms |
  | three by category, over `h` | 596 | 6 | 8.14 ms |
  | by category, over the events table | 199 | 2 | 4.09 ms |
  | by item, over the events table | 992 | 2 | 21.6 ms |
  | by embedding (`dot(h.emb, first.emb)`), over `h` | 992 | 992 | 10.5 ms |

  A grouped run costs about what reading the candidates' values does. A subquery that runs once per value costs about 10 µs a run over rows in memory, and one grouped by a key reads an `IN` list as long as the candidates (`e.item IN (...)`), which is most of its cost. A second run gave the same numbers within 0.4 ms (`chloedb/results/correlated/`).

#### Features: the values a ranking read

A ranking expression that scores each row by itself is a column, with the value the ranking computes for the row, and `features()` lists the inputs of the query's ranking, to log beside what users chose and train a model on (Vespa's match-features):

```sql
SELECT id, score(), features() FROM docs
ORDER BY bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0)) DESC LIMIT 10;
-- columns: id, score(), bm25(body, :t), rating, bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0))

SELECT id, bm25(title, :t) AS title_bm25, ln(1 + rating) AS r, year / 1000 FROM docs
ORDER BY similarity(emb, :q) DESC LIMIT 10;
```

- **`features()`** stands for one column per input, each once, named by its SQL text: every leaf of the ranking (a score such as `bm25(body, :t)` or `similarity(emb, :q)`, a column, `decay(..)`, `hops()`), the first phase's first, then each phase's expression that scores a row by itself and is not a leaf. With [rerank phases](#rerank-phases) it lists every phase's. An expression with a set function (`tmm()`, `minmax()`, `zscore()`, `rank()`), `rrf` or a weighted sum ranks rows against each other, so it is not a column: its inputs are, and `score()` is its value. A ranking by `distance(..)` lists the `similarity(..)` it ranks by. `features()` takes no alias and needs an `ORDER BY`.
- **A selected expression** may use anything a ranking expression does but the set functions: `bm25`, `similarity`, `decay`, columns, numbers, arithmetic, `ln` and the other functions, `CASE`. Its value is the ranking's, bit for bit: a row a `bm25`'s terms miss scores 0, and one without a value is `NULL`. `ORDER BY 2` ranks by the second column when it is one.
- **Over shards** a feature is one database's value: a coordinator sums the shards' BM25 statistics of the terms any column scores, and its fetch round computes the columns for the page alone, with those statistics.
- A plan with such a column needs the plan feature `rank_features`. From Python, `features()` goes in `select`, and so does any expression of scores (`select("id", bm25("title", q).alias("t"))`); Python names a feature as it writes the expression.

#### Rank profiles: a ranking kept with its table

A ranking used by many queries can be kept with its table under a name, as a search engine keeps a rank profile, and queries rank by the name:

```sql
CREATE RANK PROFILE recall ON memories AS
  WITH top AS (SELECT * FROM memories ORDER BY bm25(body, :q) DESC LIMIT 100)
  SELECT * FROM top ORDER BY bm25(body, :q) * (1 + :imp * importance) DESC
  WITH (imp => 0.2);

SELECT id, score() FROM memories WHERE kind = 'fact'
ORDER BY rank_profile('recall', q => :q) LIMIT 10;

CREATE OR REPLACE RANK PROFILE recall ON memories AS ...;   -- replaces it
DROP RANK PROFILE IF EXISTS recall ON memories;            -- ON may go when one table has it
```

- **The ranking** is a query of the table that ranks every row: `SELECT * FROM table ORDER BY expression` (any [ranking expression](#ranking-expressions), or a numeric column with `ASC` or `DESC`; `CREATE` checks that every column it names is the table's), or [rerank phases](#rerank-phases) over it. It has no `WHERE`, and no `LIMIT` after its last ranking: the query that ranks by it filters and cuts. `WITH (name => value, ...)` after it gives its parameters defaults: numbers, strings, `TRUE` and `FALSE`. Names are letters, digits and `_`, in any case; `CREATE` refuses a name the table has already, and `CREATE OR REPLACE` replaces it.
- **`ORDER BY rank_profile('name', parameter => value, ...)`** is the whole `ORDER BY` of a query of the profile's table, without `ASC`, a `WITH`, a join or `GROUP BY`. The query ranks as if it were written out: its `WHERE` filters the rows the first ranking reads, and its columns, `LIMIT` and `OFFSET` are the last ranking's, so `score()` and [`features()`](#features-the-values-a-ranking-read) work as with the ranking written out. Each parameter of the ranking is the call's argument (a value, or a parameter of the query: `q => :q`), else the profile's default, else the query's parameter of the same name.
- **Kept and replicated as writes are**: a profile is logged in its own transaction (numbered where writes are, shipped to replicas), and dropped with its table. A database with a profile needs the format 3 feature `rank_profiles`, which a version of ChloeDB from before profiles refuses by name; once the last profile is dropped and flushed, the database no longer needs it.
- **A plan names its profile** and a hash of its definition (`EXPLAIN` shows `rank profile: recall (hash ...)`) and needs the plan feature `rank_profiles`. A database whose profile of that name is another (replaced since, or a shard that missed the change) refuses the plan rather than rank otherwise. Through a coordinator, `CREATE` and `DROP RANK PROFILE` run on every shard, and the coordinator serves the profiles every replica holds alike.

#### Ranking models: `gbdt('model', features…)`

A gradient-boosted tree model, trained by LightGBM or XGBoost on the [features](#features-the-values-a-ranking-read) a query logged, ranks rows as a ranking expression of them:

```sql
SELECT id, score() FROM docs
ORDER BY gbdt('docs_ltr', bm25(title, :t), bm25(body, :t), coalesce(rating, 0), year % 6) DESC LIMIT 10;

-- as a second phase over BM25's top 200, and a binary model's probability
WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :t) DESC LIMIT 200)
SELECT id, score() FROM top ORDER BY sigmoid(gbdt('clicks', bm25(title, :t), bm25(body, :t), rating)) DESC LIMIT 10;
```

- **The model** is the file `<models>/<model>/gbdt.json` in the models directory, the one embedded fields' models come from (`--models` on a server, `models=` in Python, else `~/.local/share/chloedb/models` (or `~/.local/share/strata/models`, on a machine that has that folder from before the rename; before 0.3.2, always)): LightGBM's `booster.dump_model()` written with `json.dump` (numerical and categorical splits, any missing-value handling, a random forest too), or XGBoost's `booster.save_model("gbdt.json")` (a `gbtree`; categorical splits, multiclass and `dart` are refused). A name is letters, digits, `_`, `-` and `.`. A model is read when a query first uses it and again when its file changes.
- **Its arguments are its features**, in the order it was trained on, each any ranking expression of the row (a score, a column, arithmetic); as many as the model reads. A `NULL` feature is a missing value, which goes where the model sends missing values (LightGBM: `NaN`; or 0 under `zero_as_missing`).
- **The score is the model's raw one**, the sum of its trees, bit for bit what LightGBM's `predict(raw_score=True)` and XGBoost's `predict(output_margin=True)` give (XGBoost's in `f32`, from its base score): `sigmoid(gbdt(…))` is a binary model's probability. A random forest's score is the sum of its trees, as LightGBM's raw score is.
- **Rank it over the top rows of a cheaper ranking.** In a [rerank phase](#rerank-phases) a model is a second phase, which each shard scores on its own rows, one tree at a time over them: a LambdaMART model of 300 trees over BM25's top 100 takes 0.7 to 1.0 ms a query at the median on SciFact, NFCorpus and FiQA, against 0.02 to 0.2 ms for BM25 alone (`chloedb/results/text/bm25_gbdt.json`, a 4-core cloud VM). `ORDER BY gbdt(bm25(…), columns…)` alone is exact, a [rank-safe boost](#rank-safe-boosts) bounded by the leaves each tree can reach from the text score's bound and each column's range; but summed over hundreds of trees such a bound sits well above what rows score, so the top-k loop skips few matches and scores the rest with every tree: 5.5 to 70 ms on the same collections.
- **A plan names the model and the hash of its file** (`EXPLAIN` shows `ranking model: docs_ltr (hash …)`) and needs the plan feature `rank_models`. A database or shard whose file of that name is another refuses the plan rather than rank with another model; a plan without a hash runs with the file there. A coordinator scores the shards' rows with the model too, so it needs the same file in its own models directory.
- `features()` lists a model's inputs' leaves and then the model's score, and from Python `gbdt("docs_ltr", bm25("title", t), col("rating"))` builds the same ranking.

#### Cross-encoders: `cross_encoder('model', fields…, query)`

A cross-encoder reads the query and a row's text together and scores how well the text answers it. It costs thousands of times what BM25 does per row, so it reranks the top rows of a cheaper ranking, in a rerank phase of its own:

```sql
WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :q) DESC LIMIT 20)
SELECT id, title, score() FROM top
ORDER BY cross_encoder('ms-marco-MiniLM-L-6-v2', title, body, :q) DESC LIMIT 10;

-- mixed with other scores of the row, and a phase after it
WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :q) DESC LIMIT 20),
     ce AS (SELECT * FROM top ORDER BY cross_encoder('bge-reranker-base', body, :q) + 0.1 * coalesce(rating, 0) DESC LIMIT 10)
SELECT id, score() FROM ce ORDER BY bm25(title, :q) + decay(published, now(), 2592000) DESC LIMIT 5;
```

A server reranks at most 20 rows a query by default: start it with `--max-rerank 100` to rerank the top 100, as the measurements below do.

- **The model** is a Hugging Face directory `<models>/<model>` holding `config.json`, `tokenizer.json` and its weights. A `model.onnx`, any sequence classifier with one label exported to ONNX (int8 ones too, as Hugging Face repositories ship them under `onnx/`, such as `model_qint8_avx512_vnni.onnx`), runs through ONNX Runtime in a server or Python package built with `--features onnx`, which downloads ONNX Runtime's library when it builds and links it in; its rows are sorted by length into batches of 16, so that little of a batch is padding. A `model.safetensors`, a `BertForSequenceClassification` (such as `cross-encoder/ms-marco-MiniLM-L-6-v2`) or an `XLMRobertaForSequenceClassification` (such as `BAAI/bge-reranker-base`) with one label, runs through candle in `f32` in one built with `--features bert` (`accelerate` on a Mac), as BERT embedding models do. A directory with both runs its `model.onnx`; without the feature its weights need, a query by it is refused. `chloedb-server fetch-model ms-marco-MiniLM-L-6-v2-int8` (or `gte-reranker-modernbert-base-int8`, a ModernBERT model) downloads an int8 export ready to run. A model is read when a query first uses it and again when one of its files changes. `builtin:overlap` needs no files: the share of the query's distinct words the text holds, for tests and demos.
- **A model served elsewhere** is called over HTTP, a batch of rows a request (feature `hosted`, on by default), for models too large or slow for the server's CPU: a provider's by name, `cohere/rerank-v3.5`, `voyage/rerank-2.5` or `jina/jina-reranker-v2-base-multilingual`, with the key in `COHERE_API_KEY` (or `CO_API_KEY`), `VOYAGE_API_KEY` or `JINA_API_KEY`; or a server of one's own that a directory `<models>/<model>` names in an `endpoint.json`, such as Hugging Face TEI on a GPU (`{"api": "tei", "url": "http://gpu:8080/rerank"}`), or vLLM, Infinity or LiteLLM (`"api": "vllm"`, `"infinity"` or `"litellm"`, with a `base_url`; they answer at `/rerank` in Cohere's shape). `endpoint.json` takes the keys an [embedding model's](server.md#embedded-vector-fields) does that apply (`url` or `base_url`, `model`, `key_env`, `key_header`, `headers`, `batch`, `max_chars`, `concurrency`, `retries`, `timeout_s`), and `COHERE_BASE_URL`, `VOYAGE_BASE_URL` or `JINA_BASE_URL` sends a provider's requests to a server of its API. A request carries 1,000 rows at most (TEI's default batch, 32, for a server of one's own), four are sent at once, a rate limit or a server's error is tried twice more (pauses of at most 5 s; a request times out after 10 s), and a refused key, a server that keeps failing or an answer that misses a row fails the query. A provider's score is its `relevance_score`, between 0 and 1; TEI's is the logit (`raw_scores`), as a model run here gives it. A row with no text is not sent, and gets no score. The plan's hash is of the endpoint (its API, URL and model, not its key), so a plan made with one server is refused by another.
- **The text** is the values of the fields named, stored TEXT or KEYWORD fields, joined by a space (`NULL`s left out, each value of a list), and the query is the last argument, a string or a parameter. Query and text together are cut to 256 tokens (or the model's length, if shorter), the longer of the two first, as Hugging Face's tokenizers cut a pair; a `cross_encoder.json` in the model's directory, `{"max_tokens": 512}`, sets another length.
- **A language model** grades each row instead: `anthropic/<model>` (a Claude model, such as `anthropic/claude-haiku-5-5`, its key in `ANTHROPIC_API_KEY`), `openai/<chat model>`, or a chat server of one's own that an `endpoint.json` names with `"api": "chat"` (OpenAI's `/chat/completions`, as vLLM, Ollama, LM Studio and LiteLLM serve it; `base_url` is its `/v1`) or `"api": "anthropic"`. Each row is its own request, eight at once (`concurrency`), with UMBRELA's prompt (the one [`/v1/judge`](server.md#relevance-judgments) asks): the model reads the query and the row's text and answers a grade from 0 (nothing to do with the query) to 3 (the exact answer), which is the row's score. A model costs a request a row and about a second each, so it reranks a few dozen rows at most, and gives many rows the same grade: add the first phase's score to break ties (`cross_encoder('anthropic/claude-haiku-5-5', body, :q) + 0.01 * bm25(body, :q)`). An answer without a grade leaves the row without a score, and it ranks last; a request times out after 30 s.
- **The score is the label's logit**, what sentence-transformers' `CrossEncoder.predict` gives with an identity activation (`sigmoid()` of it is the probability for a model trained with one), equal to PyTorch's within 1e-5. A phase may mix it with other scores of the row (`cross_encoder(…) + 0.1 * rating`), and phases after it rerank the rows it kept.
- **It runs once per query, in a global phase**: on a database over the rows the phase before it kept, and through a coordinator once on the coordinator, which reads the text of the rows its shards ranked best in the fetch round of query then fetch and scores them; shards need no model. A cross-encoder is refused as the first ranking (rerank a cheaper ranking's top rows), as a column, and among `features()`.
- **Its cost is per row and per token**: on four cores `ms-marco-MiniLM-L-6-v2` takes about 50 ms a row of 256 tokens in `f32` through candle, and its int8 export through ONNX Runtime about 20 ms. BM25's top 20 takes 0.9 to 1.0 s at the median on SciFact, NFCorpus and ArguAna in `f32` and 0.37 to 0.43 s in int8, and the top 100 4.4 to 4.8 s and 1.8 to 2.0 s, where BM25 alone takes 0.1 to 1 ms; cut at 512 tokens, as before, the top 20 took 1.3 to 1.6 s ([results](results.md#cross-encoder-reranking), a 4-core cloud VM). `--max-rerank` on a server (default 20, a quarter of the default deadline at p99 in `f32`) refuses a phase that would score more rows, and the query's deadline (`--timeout-ms`, 5 s) is checked between batches of 16 rows. Its layers run on the threads the queries share (one per core), so reranks running at once take turns on the cores rather than each starting a thread per core, and queries that ask for a model not yet open wait for one of them to open it.
- **Whether it helps depends on the collection**: reranking BM25's top 100 by `ms-marco-MiniLM-L-6-v2`, a model trained on web questions, raises NFCorpus's nDCG@10 from 0.329 to 0.352, leaves SciFact's where it was (0.687 against 0.691) and lowers ArguAna's from 0.362 to 0.312; see [the results](results.md#cross-encoder-reranking). Measure it on your own queries before paying for it.
- **A plan names the model and the hash of its files** (FNV-1a of the three, or of a served model's endpoint; `EXPLAIN` shows `ranking model: … (hash …)`), with the plan feature `rank_models`, as for [`gbdt`](#ranking-models-gbdtmodel-features). From Python, `.order_by(bm25("body", q)).rerank(cross_encoder("ms-marco-MiniLM-L-6-v2", ["title", "body"], q), 100)` builds the same query.

#### The rank log: what each query served

A ranking is trained on what it served. A query run with a log writes one row per row it answered to a log table, with the values its ranking read as it computed them, so a model trained later sees what serving saw, however the tables change in between: new rows that move BM25's statistics, counters that grow, rows deleted.

```sql
-- what the first query logged to it makes (rank_log unless the log names another)
CREATE TABLE IF NOT EXISTS rank_log (
  id KEYWORD PRIMARY KEY,                 -- '<request>#<position>'
  request KEYWORD NOT NULL, ts TIMESTAMP NOT NULL, context KEYWORD,
  row_key KEYWORD NOT NULL, position INT NOT NULL, shown BOOL NOT NULL,
  score FLOAT, features KEYWORD OPTIONS(indexed = false),
  variant KEYWORD, label FLOAT,
  query KEYWORD OPTIONS(indexed = false)  -- on a request's first row: {"sql": ..., "params": ...}
) OPTIONS(cluster_by = 'context');

-- a label written later: a judge's grade, an agent citing the row
UPDATE rank_log SET label = 3 WHERE request = :r AND row_key = :k;
```

```sh
curl -s localhost:7700/v1/query -H 'content-type: application/json' -d '{"sql": "SELECT id, title FROM docs ORDER BY bm25(body, :t) * (1 + 0.1 * coalesce(rating, 0)) DESC LIMIT 10",
  "params": {"t": "mrna vaccine"}, "log": {"context": "u42", "variant": "b", "candidates": 50}}'
# {"columns": ["id", "title"], "rows": [...], ..., "request": "9f0c...", "logged": {"rows": 50, "seq_no": 812}}
```

- **A logged query** is one `SELECT` ranked by an `ORDER BY` ([rerank phases](#rerank-phases) and [rank profiles](#rank-profiles-a-ranking-kept-with-its-table) included) of a table with a primary key: `"log"` in the body of `POST /v1/query` or `/v1/sql` (`true`, or an object of options), `db.query_logged(sql, params, context=...)` in [Python](python.md#the-rank-log), `Db::query_logged` in Rust. It runs with `score()` and [`features()`](#features-the-values-a-ranking-read) added to its columns, answers with the columns it asked for and the request's id (`request`), and writes its rows to the log in one transaction once it has answered: a logged query costs a write.
- **Options**: `context`, whom the request served (a user, a session, a query class), which the export reads events by; `request`, the request's id (a random one when left out; logging a request's id again overwrites its rows at the same positions); `variant`, the client's name for the ranking that served (an A/B arm); `candidates`, a number of rows from the query's `OFFSET` to log, more than its `LIMIT` returns, those past the `LIMIT` logged as not shown (unshown negatives); `ts`, when it was served (ISO 8601 text or microseconds; now when left out), for replaying past traffic; `table`, another log table; `interleave`, a second ranking to show in the same list ([interleaving](#interleaving-two-rankings)).
- **A row** holds its request, time and context, the key of the row served (`row_key`, as text), its `position` from 0 (the `OFFSET` included), whether it was `shown`, its `score()`, and `features`: a JSON object of each `features()` column, named by its SQL text, whose numbers read back to the bits the ranking computed. The request's first row also keeps its `query`: the SQL as the client sent it and its parameters as they were bound (a vector as `{"$vector": [...]}`, a time as `{"$ts": microseconds}`), which the [relevance autopilot](#the-relevance-autopilot) replays. A table made by hand serves as a log if it has these columns, of these types (with `retain = '90 days'`, say); it may leave out `query`, and then keeps no query (a log made before the column was added: `ALTER TABLE rank_log ADD COLUMN query KEYWORD OPTIONS(indexed = false)`).
- **Through a coordinator** the query is the coordinator's, so the values are one database's ([features over shards](#features-the-values-a-ranking-read)), and the rows go to the shards their keys place them on; the first logged query makes the table on every shard. A replica refuses a logged query, the log being a write; a primary ships the table it makes to its copies, as it ships the rows. A shard's query (`"shard": true`) is not logged.
- **Cost.** Computing the values is nearly free, since the ranking computes them anyway; the write is the cost. On a 4-core cloud VM over HTTP, with 100,000 documents and one client, a query took 1.87 ms at the median plain, 1.85 ms with its values added and 4.33 ms logged (7.81 ms logging 100 candidates); writing the same ten rows alone through `/v1/write` took 1.97 ms. Without the disk's flush (`--no-sync`) the logged query took 3.50 ms: the sync is a third of the log's cost here, and the rest is the write itself. Four clients got 1,620 plain and 670 logged queries a second. A row took 152 bytes in the segments (an Intel Xeon @ 2.10GHz, 16.9 GB, Linux 6.18; `chloedb-server/results/rank_log/vm.json` and `vm-no-sync.json`, from `chloedb-rank-log-bench`). The export read 70,000 records a second, each with a history of 20 events and a label.

#### Exporting the log for training

The export reads a log table back from one snapshot, one record per logged row, with what happened around it as of its time, from a table of events (a user's views, clicks, purchases): the context's last events before the row (its history, for a sequence model), and a label from the events on the row's key after it, within a window.

```sh
curl -s localhost:7700/v1/rank_log/export -H 'content-type: application/json' -d '{"events": {"table": "events", "user": "usr", "item": "item", "time": "ts", "action": "action"},
  "history": 20, "window": "30 minutes", "grades": {"click": 1, "buy": 3}, "since": "2026-10-01"}'
# {"id": "9f0c...#0", "request": "9f0c...", "ts": 1793338200000000, "context": "u42", "row_key": "d17", "position": 0, "shown": true,
#  "score": 7.31, "variant": "b", "label": 3.0,
#  "features": {"bm25(body, :t)": 6.09, "bm25(body, :t) * (1 + 0.1 * coalesce(rating, 0))": 7.31, "rating": 2.0},
#  "history_items": ["d3", "d9", ...], "history_times": [...], "history_actions": ["view", "click", ...]}
```

- **Records** come as NDJSON from `POST /v1/rank_log/export`, as a list of dicts from `db.export_rank_log(...)` in Python, and from `Db::export_rank_log` in Rust, context by context, each context's rows in the order they were served. Times are microseconds since the epoch. `since` and `until` keep the rows served in `[since, until)`, and `table` names another log.
- **`events`** names a table, its column of contexts (`user`, `KEYWORD` or `INT`), of the rows the events were about (`item`), of when they happened (`time`, a `TIMESTAMP` or an `INT` of microseconds), and optionally of what they were (`action`). `OPTIONS(shard_by = 'usr', cluster_by = 'usr')` keeps each user's events together, on one shard and in few pages.
- **History**: with `history` set to n, the context's last n events before the row's time, oldest first. **Label**: with `window`, a period (`'30 minutes'`) or seconds, the best grade among the context's events on the row's key in the window after the row's time, `grades` giving each action's (an event whose action has none is not a label; without `grades` every event grades 1), and 0 when there is none. A label written on the row wins. A row without a context has no history and no label but its own.
- **As of the row's time**, by the events' own times: an event timed before a row is in its history even when it arrived after the row was served, which a query at the time could not have seen. The features are the values served, never recomputed.
- **Through a coordinator** the export and the interleaving credit run their queries over every shard, each read whole by a scroll, rather than from one snapshot ([cluster limits](cluster-limits.md)). Logs placed by context (a hand-made log with `context KEYWORD NOT NULL` and `shard_by = 'context'`) and events placed by user then give each shard every row and event of its contexts.

#### Interleaving two rankings

An A/B test shows each user one ranking and compares the users; interleaving shows each user both, in one list, and asks which ranking's rows the user acted on. Each request sets the two side by side for the same user and the same query, so it tells two rankings apart with ten to a hundred times fewer requests than an A/B test of the same two (Chapelle et al., "Large-scale validation and analysis of interleaved search evaluation", TOIS 2012).

```sh
curl -s localhost:7700/v1/query -H 'content-type: application/json' -d '{"sql": "SELECT id, title FROM docs ORDER BY bm25(body, :t) DESC LIMIT 10", "params": {"t": "mrna vaccine"},
  "log": {"context": "u42", "interleave": {"sql": "SELECT id, title FROM docs ORDER BY bm25(body, :t) * (1 + 0.1 * coalesce(rating, 0)) DESC LIMIT 10", "variants": ["bm25", "rated"]}}}'
# {"columns": ["id", "title"], "rows": [...], "request": "9f0c...", "teams": ["rated", "bm25", "bm25", "rated", ...], "logged": {"rows": 10, ...}}

curl -s localhost:7700/v1/rank_log/interleaving -H 'content-type: application/json' -d '{"events": {"table": "events", "user": "usr", "item": "item", "time": "ts", "action": "action"}, "window": "30 minutes"}'
# {"pairs": [{"variants": ["bm25", "rated"], "requests": 4120, "acted": 1630, "wins": [690, 820], "ties": 120, "delta": -0.040, "p": 0.00089}], "took_ms": 61.3}
```

- **The list** is team draft (Radlinski, Kurup and Joachims, CIKM 2008): both queries run, on one snapshot, and in each round the ranking with fewer rows in the list, or a coin's choice when they have as many, adds its best row not yet in it, up to the logged query's `LIMIT`. A row both rankings hold goes to the one that took it. The coin is seeded by the request's id, so the same request draws the same list. When one ranking runs out, the other's rows fill the list for neither team. `teams` in the answer names each row's ranking; show the rows, not the teams.
- **The second ranking** is a `SELECT` of the same table answering the same columns, with the same parameters. Both need a `LIMIT` and no `OFFSET` (a draft is of a first page), and `candidates` and `variant` are left out: each row shown is logged with its own ranking's score and features and its team's name as its `variant` (`variants`, `["a", "b"]` unless named).
- **The credit**: `POST /v1/rank_log/interleaving` takes the export's body and reads the log as the export does, labels included. A request is interleaved when its shown rows carry two variants, and it goes to the team whose rows its user acted on more (a label above 0), or is a tie; a request nobody acted on counts for neither. `delta` is Chapelle et al.'s Δ, (wins of the first + ties / 2) / requests acted on − 0.5, above 0 when the first ranking (by name) is preferred; `p` is the two-sided sign test of the wins, ties left out. `Db::interleaving_credit` gives the same in Rust, and the command center's Recsys eval draws it.

#### The relevance autopilot

The autopilot reads the queries the rank log kept, proposes new numbers for the [rank profile](#rank-profiles-a-ranking-kept-with-its-table) they rank by, and shows the measured before and after on the database's own data before anything changes.

```sh
curl -s localhost:7700/v1/relevance/autopilot -H 'content-type: application/json' -d '{"profile": "recall", "since": "2026-11-01", "judge": true}'
# {"report": {"verdict": "propose", "reason": "held-out nDCG@10 0.412 to 0.447 (+0.035, 95% interval +0.019 to +0.052): 61 queries better, 12 worse (sign test p = 0.000)",
#   "statement": "CREATE OR REPLACE RANK PROFILE recall ON docs AS SELECT * FROM docs ORDER BY bm25(body, :q) + 0.4 * bm25(title, :q) DESC",
#   "changes": [{"knob": "0.05 * bm25(title, :q)", "from": 0.05, "to": 0.4}], "snapshot": 81231,
#   "held_out": {"queries": 140, "before": 0.412, "after": 0.447, "better": 61, "worse": 12, "p": 0.0, "ci": [0.019, 0.052]},
#   "latency_ms": {"before": {"p50": 1.9, "p95": 4.1}, "after": {"p50": 1.9, "p95": 4.3}}, "holes": [...], ...},
#  "proposal": "6932a1f0-9e1b...", "judged": {"rows": 184, "model": "...", "errors": []}}
curl -s -X POST localhost:7700/v1/relevance/proposals/6932a1f0-9e1b.../apply
```

(The numbers above show the shape of an answer; they were not measured.)

- **The workload** is each distinct query (its SQL and parameters, named by a `qid`) that a logged request asked with `ORDER BY rank_profile(...)`, the most recent first (`max_queries`, 500), logged since `since`. The profile is the one most of them rank by unless `table` and `profile` name one. The autopilot tunes a profile because a profile is what a proposal can change; a query that ranks by an expression of its own is left out.
- **Grades** are the rank log's labels (a row's mean over the query's requests) and the judgments table's, `relevance_judgments` unless `judgments` names another, which win for the same row: `{id: '<qid>#<row_key>', qid, query, row_key, grade, source, ts}`, grades 0 to 3 (UMBRELA's scale and the relevance lab's). A query needs a row graded above 0 to count, and the autopilot says nothing with fewer than `min_queries` (20) such queries.
- **The search** is coordinate ascent (Metzler and Croft, 2007) over the profile's numbers: the defaults of its parameters (`WITH (w => 0.2)`) and the numeric literals of its rankings' `ORDER BY` expressions, 0 and 1 left out as structure. Each number is tried at 2, 0.5, 1.25, 0.8, 4 and 0.25 times its value, and a step is taken when the training queries' mean nDCG@k (`k`, 10) rises, until no step helps or `budget` (60) candidates have been replayed. A proposal is the profile with some numbers moved, nothing else.
- **The proof**: every query reads one snapshot (`snapshot` is its sequence number), so both rankings see the same rows and the same statistics. The share `held_out` (0.3) of the graded queries, picked by a seeded hash of their `qid`, is kept out of the search, and the verdict is drawn from those alone: `propose` when their mean nDCG@k rose by more than `min_gain` (0.005), more of them were made better than worse and the sign test of the two says so at `alpha` (0.05) (a few queries up by much and more down by a little is `not_significant`, though its mean rose), and the 95th percentile of the queries' time did not grow by more than `max_slowdown` (1.5 times, beyond 2 ms). Otherwise `no_change`, `no_gain`, `not_significant`, `slower`, `too_few_judged` or `no_workload`, each with a `reason`. The report has the training, held-out and overall measures, a bootstrap interval of the held-out change, each query's nDCG both ways, and the share of the top k that was graded. With the same data and `seed` a report is made again the same.
- **Rows no one graded** gain nothing, which favours the ranking now (its rows are the ones people saw and graded). The report lists them (`holes`, `max_holes`, 200); with `"judge": true` the server has Claude grade them first (UMBRELA's prompt, as `POST /v1/judge` does; the server's `ANTHROPIC_API_KEY` or `api_key`), writes the grades to the judgments table with source `claude`, and measures again.
- **Proposals** are kept in `relevance_proposals` with the report (`keep`, true): `GET /v1/relevance/proposals` lists them, `GET /v1/relevance/proposals/:id` gives one with its proof, `POST .../:id/apply` runs its `CREATE OR REPLACE RANK PROFILE` and `POST .../:id/dismiss` closes it. Applying is refused once the profile has changed since the measure (the proof is of another profile: run the autopilot again), and for a verdict other than `propose` unless `{"force": true}`. A proposal's row is not taken at its word: its statement is applied only when it is the profile now with some of its numbers moved (never another ranking, even with `force`), and its proof is made again first, on the log and the grades as they are, with the options it was measured with (kept in the proof as `options`); a proof made again that does not propose it is refused unless `{"force": true}`. Running, applying and dismissing need an admin key, and so does any write to `relevance_proposals` and `relevance_judgments` (a write key's answers 403 `scope_refused`). The command center's Labs have it as Relevance autopilot.
- **In Rust**: `Db::autopilot(&Options)` gives the report and reads only; `Db::keep_proposal`, `Db::apply_proposal` and `Db::add_judgments` write. A coordinator does not run it, since its shards hold a snapshot each: run it on a single server or on each shard's server.

#### Sequence models: `encode('model', history)`

A sequence model reads what a user did, newest first, and gives a vector in the space of an item field, so that the items it ranks first are the ones the user is likely to want next:

```sql
-- the next item for a user, from their last 200 events
SELECT id, score() FROM items
WHERE NOT id IN (SELECT item FROM events WHERE usr = :u)
ORDER BY similarity(emb, encode('next', (SELECT item, action, ts FROM events WHERE usr = :u ORDER BY ts DESC LIMIT 200), now => :now)) DESC
LIMIT 10;

-- as a feature of a ranking, next to the text's
WITH top AS (SELECT * FROM items ORDER BY bm25(title, :q) DESC LIMIT 200)
SELECT id, score() FROM top
ORDER BY bm25(title, :q) + 2 * coalesce(dot(emb, encode('next', (SELECT item, ts FROM events WHERE usr = :u ORDER BY ts DESC LIMIT 200))), 0) DESC
LIMIT 10;
```

- **The history** is a subquery of the user's events, newest first: its first column is an item's primary key in the model's table, then optionally what the event was (a text column: `view`, `buy`, a rating) and when (a `TIMESTAMP`, or a number of microseconds since the epoch, as the [rank log's export](#exporting-the-log-for-training) writes them). With two columns the second is the time when it is a `TIMESTAMP` and the action when it is text; a number there is refused, since it could be either, so give microseconds as the third column (`SELECT item, NULL, micros`). Order it by time, descending: a row newer than one above it is an error rather than a history read backwards. Of its rows whose item has a vector, the model reads the newest, up to the length it was trained on, as training reads a history. `now =>` is the time of the request (a `TIMESTAMP`, microseconds or ISO 8601 text; the clock's by default): the time since the newest event is part of what the model reads.
- **Where it goes**: `similarity(field, encode(…))` ranks the field's nearest items (the dot product with the model's vector: a search of the field's index, its best scored exactly, as [`maxsim`](#maxsimfield-vectors-several-query-vectors) runs one), `maxsim(field, encode(…))` the same as one query vector, and `dot(field, encode(…))` is a number in any ranking expression. A user with no events has no vector: `similarity` and `maxsim` then return no rows, and `dot` is `NULL`. The history runs once a query, however often `encode` appears in it.
- **The model** is a small causal transformer (SASRec's): each event is its item's vector, projected, plus a learned vector for its action, for the time until the next event (in powers of two of seconds) and for its position; two layers of attention over the events, each seeing only the ones before it, then the last event's output, projected into the item field's space and normalized. It is the directory `<models>/<model>/` (`config.json`, `model.safetensors` and `items.safetensors`), in the models directory [`gbdt`](#ranking-models-gbdtmodel-features)'s files come from. The config names the table and the vector field the model was trained on and their dimension: a database without that field, or with one of another dimension, refuses the model, and so do `similarity` and `maxsim` over another field. `items.safetensors` holds the item vectors it was trained with, and is part of the model's version (its hash): a history whose items have other vectors in the field is refused, naming one, since the model's vector would be in a space it never saw. An item the file does not list (one added since) is read with the field's vector.
- **Training** is `chloedb.train.sequence` in Python, with PyTorch (`pip install torch`), from a table of events, the rank log's export, or lists of `(item, action, time)`:

  ```python
  import chloedb, chloedb.train
  db = chloedb.Database.open("shop.chloedb")
  out = chloedb.train.sequence(db, "next", table="items", field="emb",
                              events={"table": "events", "user": "usr", "item": "item", "action": "action", "time": "ts"},
                              learn_items=True)
  chloedb.train.write_vectors(db, "next")   # with learn_items, the items' learned vectors into items.emb
  ```

  It trains next-item prediction over each user's events, with every item in the field (`negatives="all"`) or the other targets of a batch (`"in_batch"`, for many items; with `logq=True` and `uniform_negatives=n` it no longer learns to rank popular items low) as the negatives, and writes the model where `models=` says (the models directory by default), with the item vectors it was trained with. With `learn_items=True` the item vectors are trained too, starting from the field's: `chloedb.train.write_vectors` writes them into the field, changing nothing else of each row, and until then `encode` refuses the model. Otherwise the field's vectors are kept as they are and only the model learns.
- **Equal to PyTorch's**: ChloeDB's vector is the one the PyTorch module gives for the same history, within 2·10⁻⁵ (`chloedb/tests/sequence.rs`), and the items a query ranks are the ones PyTorch's vector ranks.
- **How well it ranks**: on MovieLens-1M and Amazon Beauty, leave-one-out over every item as SASRec is measured, a model of two layers that reads items only has Recall@10 0.278 and 0.066 (NDCG@10 0.159 and 0.041). That is above the original SASRec's published 0.250 and 0.061, but below SASRec trained the same way as this model, with cross-entropy over every item (SASRec+, 0.315 on MovieLens), and on Beauty below counted transitions from the same events ([`chloedb.next_items`](next-items.md), 0.069): good, not the best published. Reading times too, it has 0.319 and 0.077. Both MovieLens runs stopped at a time limit while still improving. ChloeDB's answers equal PyTorch's on every user ([`results/sequence`](https://github.com/chloetech-ai/chloedb/tree/main/chloedb/results/sequence), with the protocol and the published numbers), and CI checks ranking quality on MovieLens-1M against popularity and item-kNN baselines and ChloeDB's other sources (`chloedb/results/sequence/gate.py`): there a model trained for an hour has Recall@10 0.306, `ppr` over what came after what 0.242, counted transitions 0.211 and item-kNN 0.069.
- **What it costs**: the model alone, on one thread of a cloud VM's Xeon at 2.80 GHz, takes 0.09 ms for 10 events, 0.28 ms for 50 and 1.5 ms for 200 (p50); the whole query, reading the history and searching the field, 0.6 to 0.7 ms on Beauty's 50 events and 2 to 3 ms on MovieLens's 200. Keep `max_len` as short as the ranking allows.
- **A plan names the model and the hash of its files** and needs the plan feature `encode`; a database or shard whose files of that name are others refuses it. A coordinator runs `encode` itself, from the history its shards return, so it needs the model; the shards don't.

### `decay(field, origin, scale, …)`: closeness to a value

A numeric column's closeness to `origin`, as in Elasticsearch's decay functions. With `d = max(0, |x − origin| − offset)`, the score is 1 at `d = 0` and `decay` at `d = scale`:

| `shape` | score |
|---|---|
| `'exp'` (default) | `decay^(d / scale)` |
| `'gauss'` | `decay^((d / scale)²)` |
| `'linear'` | `max(0, 1 − (1 − decay) · d / scale)`, reaching 0 at `scale / (1 − decay)` |

Options: `offset => 0`, `decay => 0.5`, `shape => 'exp'`. Rows without a value score 0. `now()` as the origin is the current Unix time in seconds, so `decay(published, now(), 86400)` halves a row's score for each day of age. `decay` ranks on its own, inside `rrf` and weighted sums, and in products.

### Products: boosting relevance by a row's values

Multiplying scores boosts relevance by what a row holds:

```sql
-- fresh news first: BM25 halved at 30 days old
SELECT id FROM news ORDER BY bm25(body, :t) * decay(published, now(), 2592000) DESC LIMIT 10;

-- similar products, weighted by their rating
SELECT id FROM products ORDER BY similarity(emb, :q) * rating DESC LIMIT 10;
```

A product's factors are scores (`bm25`, `similarity`, `ppr`, `decay`) and numeric columns, one of them a relevance score (`bm25`, `similarity`, a graph's); a product of a row's own values alone (`rating * clicks`, `decay(..) * rating`) is arithmetic, and ranks as the numbers multiply. Candidates are the top `depth` (100) rows of each relevance factor, scored exactly by every factor; a product of one text score and row values (`bm25(..) * decay(..)`, `bm25(..) * rating`) is a [rank-safe boost](#rank-safe-boosts) and ranks every row the text matches. A factor below 0 (a negative similarity) or missing (a `NULL` column) counts as 0, so the product of two negatives cannot rank high. Over shards, each shard returns its factors' raw scores and the coordinator multiplies them, so a split collection ranks like the whole one.

### A column

`ORDER BY year DESC` orders by a numeric column. Rows without a value come last in either direction (as in Elasticsearch); `ORDER BY year DESC NULLS FIRST` puts them first. Their `score()` is `NULL`. With no `ORDER BY`, rows come in storage order.

With a `LIMIT`, the rows can come from the column's blocks of 128 rows, best block first by its zone map (its largest value for `DESC`, smallest for `ASC`), stopping at the first block whose best value is worse than the k-th row found. A row's filter is checked only when its value can still make the cut, and blocks the filter's own zone maps rule out are never read. So the ten most clicked of the 560,000 ORCAS queries starting with `a` read about 150 of 81,000 blocks, and about 10 when the table is clustered by query (`OPTIONS(cluster_by = 'id')`), where each prefix's rows sit together. The planner keeps the plain scan of the matches when they are a few thousand rows, or under 1 in 64 of the rows the zone maps admit (a rare prefix of an unclustered table, whose k-th best value is low enough that most blocks could hold a better one), and switches to it when the blocks have cost half of what it would (a filter that rejects the best values, `WHERE n < 3 ORDER BY n DESC`); `NULLS FIRST` always scans. Values written in place are read first, so counters kept with `UPDATE ... SET n = n + 1` rank by their current value.

## Typos and synonyms

`MATCH` and `bm25` take two options that widen a search beyond the words written:

```sql
SELECT id FROM papers WHERE MATCH(body, 'nueral netwrok', fuzzy => 'auto') LIMIT 10;
SELECT id, score() FROM papers ORDER BY bm25(body, 'nueral netwrok', fuzzy => 1) DESC LIMIT 10;
SELECT id FROM papers WHERE MATCH(body, 'gnn', synonyms => 1) LIMIT 10;
```

- **`fuzzy => 1` or `2`** also matches the indexed terms within that many edits of each word, counting an insertion, deletion, substitution or swap of two neighbouring letters as one edit (optimal string alignment). **`fuzzy => 'auto'`** allows no edits for words of 1 or 2 letters, 1 for 3 to 5 letters and 2 for longer ones. In the `MATCH` grammar, `word~` is `'auto'` for one word and `word~1`, `word~2` choose. A ranked query adds the 50 nearest terms of each word, each weighted `1 − edits / length` (at least 0.1), as Lucene's fuzzy queries do. A word and its typo neighbours share one document frequency, the largest among them, so a rare misspelling scores below the word rather than above it; the expansion is found once per query over every segment's dictionary, so all segments, and the plan line `bm25 expanded with N terms`, agree. Each dictionary is walked as a trie, skipping every run of terms whose start is already too many edits away, rather than tested term by term. A skip inside a block of the front-coded dictionary passes terms by their lengths alone, and each letter's row of edits is worked out for up to 63 letters of the word at once in a machine word (Wu and Manber's bit-parallel matching, with Hyyrö's swap of two letters). For a word of a typed query looked up in the 797,472 analysed terms of ORCAS's 10.4 million queries, the median lookup takes 1.4 ms with one edit and 7.8 ms with two (2.1 and 16.5 ms before the in-block skip and the bit-parallel rows, run just before on the same machine), against 77 and 106 ms testing every term (one thread of a 4-core cloud VM, Intel Xeon @ 2.10GHz; [`results/text/fuzzy-orcas.json`](https://github.com/chloetech-ai/chloedb/tree/main/chloedb/results/text), `chloedb-fuzzy-bench`). A query walks a copy of each segment's terms laid out flat in memory rather than the dictionary itself: each term whole, where it ends and how many bytes it shares with the one before, so a step decodes nothing and a jump past a run of terms reads only those shared counts. The copy is made at the segment's first typo query (12 ms for 530,000 terms) and kept while the segment lives, at the terms' bytes plus 5 bytes a term (6.5 MB for 2 million MS MARCO passages, against the dictionary's 4.7 MB). On a shard of 5 million MS MARCO passages in three segments, with the engine comparison's typed queries, a word's walks over the three segments take 9.4 ms against 22.6 ms over the dictionaries, the median typed query 17.0 ms against 29.1 ms, and four clients, one thread each, get 215 typed queries a second against 124 (a 4-core cloud VM, Intel Xeon @ 2.80GHz).
- **A ranked query with `fuzzy => 'auto'` corrects typos** rather than widening every word, by how many documents hold the word and its neighbours, as a spelling corrector does. A word in at least 1 in 10,000 of the documents, and in 2 or more, is taken as spelled right and gets no neighbours: typos are rare words. Another word gets only its neighbours in more documents than it, each weighted, besides `1 − edits / length`, by the chance it was meant: `x / (x + df)` with `x = df′ × 0.01^edits`, where `df` counts the word's documents and `df′` the neighbour's (a noisy channel that takes each term as likely as its documents and each edit as a 1 in 100 slip). A word in no document is a typo of each of its neighbours (chance 1), and a word nearly as common as its neighbour almost surely means itself. A shard chooses the neighbours by its own counts, the same in a coordinator's statistics round as in its query, and takes the chances from the collection's counts the coordinator sends. `MATCH`, and `bm25` with `fuzzy => 1` or `2`, still widen every word.
- **A `bm25` of the text a `MATCH` filters by takes the `MATCH`'s `fuzzy`** when it names none: `WHERE MATCH(title, 'wireles hedphones', fuzzy => 'auto') ORDER BY bm25(title, 'wireles hedphones') DESC` ranks the rows the filter found, rather than scoring the words as written and dropping them all. The text must be the same (case and outer spaces aside), on the same field.
- **A typo is looked up as typed too**: under a stemming analyzer such as `english`, a word the index does not hold looks for neighbours of the word as written as well as of its stem, so `wireles` (stem `wirel`, three edits from `wireless`) finds `wireless` one edit away.

  On 500,477 passages, half of MS MARCO's first million, with the first 300 dev queries, each given a typo as the engine comparison does (two letters of its longest word swapped; 271 queries take one), the typed query's top 10 holds 0.685 of the top 10 that plain `bm25` gives the query as written, against 0.346 widening every word and 0.383 without typo tolerance. On the queries as written, `'auto'` keeps 0.946 of plain `bm25`'s top 10, against 0.340 widening every word. The median typed query takes 10.0 ms, against 48.5 ms before this change and 20.6 ms widening every word on its faster walk and scoring, and the median query as written 0.35 ms (19 ms widening every word), since it walks no dictionary. Through a coordinator over two shards of the million passages (both halves, on the same machine), the median typed query takes 15.8 ms against 112 ms before (p99 30 against 287 ms), and four clients get 137 typed queries a second against 17.7: a shard also walks once per query rather than twice, its statistics round and its query sharing the walk. On BEIR's SciFact and NFCorpus, whose queries are spelled right, nDCG@10 with `'auto'` went from 0.673 and 0.294 widening every word to 0.693 and 0.331, level with plain `bm25` (0.691 and 0.329). The thresholds were chosen on MS MARCO's dev queries, not BEIR's test queries. One segment on a 4-core cloud VM, Intel Xeon @ 2.10GHz; `cargo run --release --example typo_queries`.
- **`synonyms => 1`** also matches what the server's synonym list says a word or phrase means. The list is in Solr's format: `laptop, notebook, portable computer` makes each find the others, and `tv => television` makes `tv` find `television` but not the reverse. Both sides are analysed like documents, so `laptops` finds `notebook`. A one-word synonym of a word shares that word's document frequency, as Lucene's synonym queries do; the words of a longer synonym score on their own. [chloedb-server](server.md) loads it from `--synonyms FILE` and edits it at `/v1/synonyms`; from Rust, set `ExecOptions::synonyms`. Without a list, `synonyms => 1` is an error that says so.
- **A table's own list** (experimental). `CREATE SYNONYMS ON products AS 'sofa, couch\ntv => television'` gives a table rules of its own, which `synonyms => 1` on that table's searches uses after the server's list. It is kept as the stored function `synonyms_products()`, so it has versions (`CREATE OR REPLACE SYNONYMS` makes the next, `CREATE OR REPLACE FUNCTION synonyms_products FROM VERSION 1` goes back) and a coordinator writes it on every shard; `DROP SYNONYMS [IF EXISTS] ON products` removes it. The rules are checked when the list is made. `GET /v1/synonyms` lists each table's.
- **Mined synonyms** (experimental) are proposals for that list, never applied by themselves: `Db::mine_synonyms` or `POST /v1/synonyms/mine` reads the words of a stored text field and embeds the 3,000 most frequent with the model of one of the table's vector fields, proposing pairs whose vectors are close (cosine 0.6 or more) and whose spellings are not alike (a model of word pieces puts `adel` by `adela`; numbers such as `b10` and `b11` still pass); or reads a table of searches (`context`, `ts` in seconds, `text`) and proposes `a => b` where people of two or more contexts rewrote one word of a search into another within a minute. On Wayfair's WANDS (`chloedb-synonyms-eval`, `chloedb/results/text/synonyms_wands.json`, an Intel Xeon at 2.10 GHz, 4 cores), the words of 42,994 product names embedded by potion-retrieval-32M gave pairs such as `kids, children`, `bike, bicycle` and `picture, photo` beside related words that are not synonyms (`animal, dog`, `hawaii, honolulu`), and none of the lists made a measurable difference to nDCG@10 (0.4327 without; 0.4343 with the 33 rules at cosine 0.6, p = 0.31), while the 462 rules at 0.4 lowered it to 0.4231 (p = 0.007). Read the proposals before keeping them. A mining keeps to bounds whoever asks: 200,000 rows read, 5,000 words compared, a cosine of 0.3 or more and 10 proposals a word at most; on a server it takes an admin key.

## Query expansion: `expand`

> **Experimental.** This is new in 0.3.0, and its options and answers may change in a later release.

`bm25(body, :q, expand => 10)` ranks by the query's words and by the words its first ranking's top 10 rows hold most: pseudo-relevance feedback (RM3). It finds rows that use other words for what the query asks, such as `antibodies` for `vaccines`.

```sql
SELECT id, title FROM papers ORDER BY bm25(body, :q, expand => 10) DESC LIMIT 10;
SELECT id, title FROM papers WHERE year >= 2023
ORDER BY bm25(body, :q, expand => :n, expand_terms => 10, expand_weight => 0.7) DESC LIMIT 10;
```

- **How.** The query runs in two steps. The first ranks the rows as the call would without `expand`, through the query's `WHERE`, and reads the field's text of the top `n` (`SELECT body, score() FROM papers WHERE year >= 2023 ORDER BY bm25(body, :q) DESC LIMIT 10`). Each of those rows weighs its terms by its share of the scores and each term's share of the row; the `expand_terms` best terms over all of them share `1 − expand_weight` of the weight, and the query's own terms share `expand_weight`. The second step is the query with the call rewritten to those analysed, weighted terms (`bm25(body, 'vaccin^0.85 antibodi^0.03 immun^0.03 ...', syntax => 'terms')`), and the answer's plan lines start with what was added: `bm25(body) expanded from 10 rows of papers with 20 terms: antibodi 0.031, immun 0.029, ...`.
- **Options.** `expand` is the rows read, from 0 (no expansion, the call as written) to 100; `expand_terms` the terms added, up to 100; `expand_weight` the query's own share, from 0 to 1. Each may be a parameter (`expand => :n`). A call that expands takes no `min`, `min_rows` or `max_rows`.
- **Where.** In a `SELECT` of one table, in its columns, `WHERE` or `ORDER BY`, and in a `WITH` or a subquery of one table. A join is refused, and so is `expand` in a rank profile or a stored function. On a coordinator over shards the first step is an ordinary query of the shards, merged, and the coordinator rewrites the query, so every shard ranks by the same terms.
- **Cost.** A second query and the analysis of `n` rows' text: on BEIR's SciFact, NFCorpus and ArguAna the median query took 1.8, 1.7 and 2.9 ms against 0.09, 0.03 and 0.77 ms for plain `bm25` (one core of a 4-core cloud VM, Intel Xeon @ 2.10GHz).
- **Quality.** The defaults (10 rows, 20 terms, 0.85) were chosen on SciFact's train and NFCorpus's dev judgements (`chloedb/results/text/bm25_rm3_tuning_*.json`). On the test judgements, nDCG@10 went from 0.691 to 0.693 on SciFact (p = 0.55), 0.329 to 0.346 on NFCorpus (p = 0.0001) and 0.362 to 0.369 on ArguAna (p = 0.0006) (`chloedb/results/text/bm25_rm3.json`, `chloedb-bm25-eval --variants plain,rm3`). Expansion helps most where queries are short and documents use a field's own vocabulary; a setting that keeps less of the query (0.5) lowered SciFact to 0.646, so measure before lowering `expand_weight`.

## Understanding a query's words: `understand`

> **Experimental.** This is new in 0.3.0, and its options and answers may change in a later release.

`bm25(field, :q, understand => 'boost')` reads the words of a search for the values of the table's `KEYWORD` fields they name (`white`, a colour; `coffee table`, a class) and for ranges of numbers (`under $300`), and ranks by the text and by them:

```sql
SELECT id, name FROM products
ORDER BY bm25(body, :q, understand => 'boost', understand_fields => 'class, color, price') DESC LIMIT 10;
-- :q = 'white coffee table under $300' ranks as
-- bm25(body, 'white coffee table') * (1 + 0.25 * (CASE WHEN color = 'white' THEN 1 ELSE 0 END
--   + CASE WHEN class IN ('Coffee Tables', 'Coffee & Cocktail Tables') THEN 1 ELSE 0 END
--   + CASE WHEN price < 300 THEN 1 ELSE 0 END))
```

- **What names a value.** A field's values are read with a query (`SELECT color, COUNT(*) ... GROUP BY color`, once per version of the table but at most once a second for a table that takes writes, one query at a time reading it while the others use the one before; on a coordinator once a minute) and analysed as the text field is. Words name a value when they are a run of its words, in its order, that ends with its last word, its head: `coffee table` names `Coffee Tables` and `Coffee & Cocktail Tables`, and `chair` every value that ends in chairs, but `table` does not name `Table Lamps`. A run inside a longer one of another field is that one's (`coffee` of `coffee table` is not the colour coffee). Values of several fields named by the same words are one condition, any of them.
- **A check against the rows.** A run counts only when, of the rows whose text holds its words, at least `understand_share` (0.05) hold one of its values: `up` is not the brand Up when almost no row that says up is of it. That is one query a run, counting both (`SELECT COUNT(*), SUM(CASE WHEN .. THEN 1 ELSE 0 END) ... WHERE MATCH(..)`), and runs of the same words and values share one. A text is understood in its first 32 words, and a call uses its first 16 runs, so that a pasted page costs what a query does.
- **Ranges.** `under`, `below`, `less than`, `up to`, `over`, `above`, `more than`, `at least`, `between .. and ..`, `since`, `after` and `before` followed by a number (`$1,500`, `2.5`, `10k`) are a range of a numeric field named in `understand_fields`: the one named next to them (`over 10 citations`), a field named for years for a year (`since 2020`), or the only one. Their words leave the ranked text.
- **Modes.** `'boost'` multiplies the text score by `1 + understand_weight` (0.25) for each condition a row meets, a [rank-safe boost](#rank-safe-boosts). `'filter'` also puts each run that names a value in full, and each range, in the `WHERE` (a row without a value of the field is kept). `'off'` is the call as written. Each option may be a parameter (`understand => :mode`).
- **Fields.** Without `understand_fields`, every `KEYWORD` field of the table but its key, and a field of more than 20,000 values is left out as a name rather than a facet (its read stops at 20,001); a listed field reads up to 200,000. A call that understands ranks the rows of one table, without a join, and goes with `expand` (it understands first).
- `Db::understand(table, field, text, options)` (chloedb-server's [`POST /v1/understand`](server.md)) returns what a text says without ranking: its runs, the values each names, their shares, the ranges and the conditions.
- **Measured.** On Wayfair's WANDS (42,994 products, 480 queries; name and description as the text, class, category, colour, style, material, shape and product type as keyword fields) `'boost'` moved nDCG@10 from 0.707 to 0.731 on the odd-numbered queries (p = 0.002), and the classes its words named held the class the query asks for in 262 of 343 queries; `'filter'` lowered it to 0.631, since a product judged relevant often lacks the value or holds another. On Amazon's ESCI (its 8,956 US test queries of the small version and the 164,900 products judged for them; brand and colour listed) `'boost'` moved it from 0.394 to 0.396 (p = 0.007): its titles already hold their brands. Without the check against the rows, ESCI's ranking fell to 0.391. The weight and share were chosen on the even-numbered queries of both. A query took 0.9 to 1.0 ms at the median, against 0.07 to 0.12 ms for `bm25` alone (`chloedb-understand-eval`, `chloedb/results/text/understand_wands.json` and `understand_esci.json`, Intel Xeon @ 2.10GHz, 4 cores).

## Did you mean: correcting a query's words

> **Experimental.** This is new in 0.3.0, and its options and answers may change in a later release.

`Db::correct(table, field, text)` (chloedb-server's [`POST /v1/correct`](server.md)) returns the text with the words that look like typos of a TEXT field's terms replaced, for a search box to offer as "did you mean":

```json
{"text": "mRNA vaccines for neural networks", "changed": true,
 "words": [{"word": "vacines", "term": "vacin", "to": "vaccin", "to_word": "vaccines", "edits": 1, "chance": 0.86}, ...]}
```

- A word is corrected only when no row holds it: a word some document uses is a word of the collection. Words with digits or capitals inside (`p150n`, `dLNs`) are names and codes, and stay.
- Its neighbours are the field's terms one edit away (up to 5 letters) or two (longer), as `fuzzy => 'auto'` finds them, and it takes the one most likely meant if that chance is at least 0.5: the noisy channel of [typos](#typos-and-synonyms), with an unseen word counted as in 0.05 documents, so that a neighbour one edit away must be in 5 or more documents. The word shown keeps the typed word's ending on the corrected term (`ifnlammation` gives `inflammation`).
- On BEIR's queries with a typo typed in (two letters of the longest word swapped), it corrected 0.73, 0.66 and 0.85 of them back to the words as written on SciFact, NFCorpus and ArguAna; of the queries it changed, 0.91, 0.90 and 0.98 were corrected back; and it changed 4.0%, 2.8% and 0% of the queries as written (`chloedb/results/text/bm25_did_you_mean.json`, variant `typed_corrected`). For ranking, `fuzzy => 'auto'` stays better than ranking the corrected text: nDCG@10 0.692 against 0.675 on SciFact, as it widens each word with its likely neighbours rather than choosing one.
- A coordinator corrects as one server does, by its shards' document counts summed: the words' own, then, for a word in none, those of its neighbours (each shard's nearest 50 within the edits allowed).

### Times later and earlier: `INTERVAL`

```sql
SELECT host, COUNT(*) FROM syslog WHERE ts > now() - INTERVAL '15 minutes' GROUP BY host;
SELECT id FROM events WHERE at >= date_trunc('day', now()) - INTERVAL '7 days';
SELECT id, due + INTERVAL '2 days' AS reminder FROM tasks;
SELECT TIMESTAMP '2026-01-31' + INTERVAL '1 month';   -- 2026-02-28T00:00:00Z
```

- **`INTERVAL 'text'`** is a length of time: numbers each with a unit, `'15 minutes'`, `'1 day 12 hours'`, `'-2 weeks'`, `'90s'`, or `INTERVAL '15' MINUTE`. The units are `microseconds`, `milliseconds`, `seconds`, `minutes`, `hours`, `days`, `weeks`, `months` and `years`, singular or plural, and `us`, `ms`, `s`, `sec`, `min`, `m` (a minute), `h`, `hr`, `d`, `w`, `mon` and `y`. It is added to a time or subtracted from one; alone it is an error.
- **A time written as a value** (`now()`, `CURRENT_TIMESTAMP`, `TIMESTAMP '2026-10-01'`, `CAST('...' AS TIMESTAMP)`, `date_trunc('day', now())`) moved by an interval is one value, computed once for the statement: `ts > now() - INTERVAL '2 hours'` is a range of `ts`, which the index answers, in `WHERE`, `ON` and a join's side alike. Months and years move it on the calendar, a day past the new month's end becoming its last day (January 31 plus a month is February 28 or 29), as in PostgreSQL.
- **A column moved by an interval** (`due + INTERVAL '2 days'`) is computed row by row, in microseconds; a month or a year has no fixed number of them, so an interval with months moves only values. A `NULL` time gives `NULL`.
- **A number added to a time** written as a value is an error that points to `INTERVAL` (`now() - 3600` could mean seconds or microseconds). Over the rows, a number added to a `TIMESTAMP` column counts microseconds: `at + 3600000000` is an hour later.

## Aggregates

```sql
SELECT category, COUNT(*) FROM docs
WHERE MATCH(body, 'india') AND year >= 2000
GROUP BY category ORDER BY 2 DESC LIMIT 5;

SELECT category, topic, COUNT(*) AS n, AVG(rating), MIN(year), MAX(year), COUNT(DISTINCT source)
FROM docs WHERE year >= 2000
GROUP BY category, topic HAVING n > 20 ORDER BY AVG(rating) DESC LIMIT 10;

SELECT COUNT(*), SUM(clicks), AVG(rating) FROM docs WHERE MATCH(body, 'india');

SELECT item, MEDIAN(price), PERCENTILE_CONT(0.9) WITHIN GROUP (ORDER BY price) AS p90
FROM snapshots GROUP BY item HAVING MEDIAN(price) > 100 ORDER BY p90 DESC;

SELECT host, COUNT(*) FILTER (WHERE severity = 'error') AS errors, COUNT(*) AS total
FROM syslog GROUP BY host ORDER BY errors DESC LIMIT 10;

SELECT brand, AVG(price), STDDEV(price), CORR(rating, price) FROM products GROUP BY brand;
```

- **`GROUP BY`** takes one or more indexed `KEYWORD`, `INT` or `FLOAT` columns, or [parts of times](#parts-of-times-date_trunc-extract-and-strftime) of a `TIMESTAMP` column (`date_trunc('day', at)`), by name, position or alias. A multi-valued column puts a row in one group per value, over one table, joined rows or derived rows alike, and so does `SELECT DISTINCT`, which is `GROUP BY` its columns. The rows without a value form the group `NULL`, as in SQL, and `SELECT DISTINCT` lists `NULL` once.
- **Aggregates** are `COUNT(*)`, `COUNT(f)`, `COUNT(DISTINCT f)`, `SUM(f)` and `AVG(f)` over numeric columns, `MIN(f)` and `MAX(f)` over numeric or keyword columns, the [percentiles](#percentiles) of numeric columns, and the statistical aggregates below. `COUNT(f)`, `COUNT(DISTINCT f)`, `MIN(f)` and `MAX(f)` of a stored `TEXT` column (or a column kept without an index) count and compare its stored values, read with the rows. Every selected column is a group column or inside an aggregate. Without `GROUP BY` they summarize every row that passes `WHERE`, in one row (`COUNT(*)` 0 and the others `NULL` when none does).
- **`FILTER (WHERE condition)`** after an aggregate aggregates only the rows the condition holds for: `COUNT(*) FILTER (WHERE severity = 'error')` counts the errors of each group, beside `COUNT(*)` of all its rows. It is the aggregate of a `CASE` (`COUNT(CASE WHEN severity = 'error' THEN 1 END)`), and any aggregate takes it, in `SELECT`, `HAVING` and `ORDER BY`; a column without an alias is named as written. `FILTER` on a function that is not an aggregate, or on a [window](#window-functions) aggregate, is an error.
- **Statistical aggregates**: `STDDEV(x)` and `STDDEV_SAMP(x)`, the sample standard deviation; `STDDEV_POP(x)`; `VARIANCE(x)` and `VAR_SAMP(x)`, the sample variance; `VAR_POP(x)`; `COVAR_SAMP(y, x)` and `COVAR_POP(y, x)`; and `CORR(y, x)`, the Pearson correlation. As in PostgreSQL, `NULL`s are left out (the two-variable ones take the rows with both values), the sample forms are `NULL` for fewer than two values and the population forms for none, and `CORR` is `NULL` when either variable does not vary. They are computed from sums and counts in one pass (the sum of squares less the square of the sum over n), so they merge over shards as `SUM` and `COUNT` do, and with values far larger than their spread (times in microseconds) lose digits to rounding.
- **`HAVING`** filters groups by their aggregates, written out or by alias, with comparisons, `BETWEEN`, `AND`, `OR` and `NOT`, in three-valued logic: an aggregate of no values (`AVG(rating)` of a group without ratings) is `NULL`, so neither `AVG(rating) < 3` nor `NOT (AVG(rating) < 3)` keeps that group.
- **`ORDER BY`** a group column, an aggregate or a position, with a tie-break; without one, the largest groups come first. `LIMIT` and `OFFSET` apply to groups.

A `SUM` or `AVG` of `FLOAT` values is exact: the values are added without rounding and the total is rounded once, so it does not depend on the order rows are read in. A database groups its segments on several threads and merges them, and the answer is the same bits as on one thread. A coordinator over shards merges partial aggregates (sums and counts for `AVG`, the distinct values for `COUNT(DISTINCT)`), so the answers are the same as on one server, except that each shard sends its `FLOAT` sum rounded once, so a sum over shards can differ from one server's in its last bit.

### Percentiles

`PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY f)` is the value a fraction `p` (0 to 1) of the way from the group's least value of `f` to its greatest, interpolated between the two values around that position, a `FLOAT`; `MEDIAN(f)` is `PERCENTILE_CONT(0.5)`. `PERCENTILE_DISC(p) WITHIN GROUP (ORDER BY f)` is the least value that at least a fraction `p` of the values are at most: one of the values, of `f`'s type (a `TIMESTAMP`'s times too). The answers are PostgreSQL's: of `n` values in order, `PERCENTILE_DISC(p)` is the one at position ⌈p·n⌉ (counting from 1, the first for `p` = 0), and `PERCENTILE_CONT(p)` the one at position p·(n − 1) counting from 0, interpolated in doubles. `NULL`s are left out, and a group without values gives `NULL`. A column without an alias is named as the call reads, `PERCENTILE_CONT(price, 0.9)`.

- **Of columns and expressions.** Over one table they take indexed `INT` and `FLOAT` columns, read from the columns as the other aggregates are; over joined or derived rows, any numeric expression (`MEDIAN(s.price * 1.2)`). `p` is a constant or a parameter (`:p`); the order is ascending (`ORDER BY f DESC` is an error: write `1 - p`). They are aggregates of groups, not window functions.
- **Exact, so they hold the values.** A group keeps each of its values (8 bytes each, counted against the [query's memory budget](server.md#limits)) and orders them once when it is read: a median over 5 million rows holds 40 MB. A shard sends a group's values to the coordinator as runs of equal values, so prices that repeat travel once with their count, and the coordinator's answer is one server's. Plans with a percentile carry the plan feature `percentiles`, which a version of ChloeDB from before them refuses by name.

### Approximate percentiles

```sql
SELECT date_bin('5 minutes', ts) AS b, service, approx_percentile(latency, 0.99) AS p99
FROM logs WHERE ts >= '2026-11-12' GROUP BY b, service;
```

`approx_percentile(f, p)` is `PERCENTILE_DISC(p)` of the group's values (the one at position ⌈p·n⌉ in order) within 1% of it, a `FLOAT`, from a [DDSketch](https://arxiv.org/abs/1908.10693) of the group's values rather than the values themselves. `p` = 0 and 1 give the least and greatest values exactly. It takes what the exact percentiles take, `INT` and `FLOAT` columns over one table and numeric expressions over joined or derived rows, except `TIMESTAMP`s.

- **A few KB a group, however many rows.** A sketch counts each value in a bucket of values within 1% of each other, and keeps at most 1,024 buckets of each sign (4 KB): the bound holds for values up to about 10⁸ times apart, as latencies are, and past that the least ones are folded together, so the low percentiles of such a spread lose it first. A p99 over 100 million rows holds a few KB rather than 800 MB.
- **Exact merges.** Sketches of segments, threads and shards add up to the sketch of all the values, so the answer does not depend on how rows were split: a coordinator's answer is one server's. A shard sends a group's sketch as versioned bytes (base64 text), which a coordinator reads only in the version it knows. Plans with one carry the plan feature `approx_percentiles`, which a version of ChloeDB from before it refuses by name.

### Rollups: `CREATE ROLLUP`, a materialized view of counts over time

> **Experimental.** Rollups are new in 0.3.1, and their options, columns and refresh may change in a later release.

A rollup is a table of counts, sums, least and greatest values per bucket of time and per group, kept up to date from another table as rows arrive: ChloeDB's materialized view (a continuous aggregate) of a `GROUP BY` of time. Precomputed daily or hourly counts, kept up to date without a job of your own, are one: a time-bucket query over weeks of logs reads a few thousand rows of the rollup instead of millions of the table.

```sql
-- Orders per day and status, kept up to date as orders arrive.
CREATE ROLLUP orders_daily ON orders AS SELECT date_trunc('day', created_at), status, COUNT(*);
SELECT bucket, status, count FROM orders_daily ORDER BY bucket DESC;
```

It needs an admin key that is not limited to tables, and `check_sql` checks one without making it.

```sql
CREATE ROLLUP logs_5m ON logs OPTIONS(lateness = '10 minutes', retain = '400 days') AS
  SELECT date_bin('5 minutes', ts), service, severity, COUNT(*), SUM(latency), COUNT(latency), MAX(latency) AS worst
  WHERE severity <> 'DEBUG';

SELECT date_trunc('hour', bucket) AS h, service, SUM(count), SUM(sum_latency) / SUM(count_latency) AS mean, MAX(worst)
FROM logs_5m WHERE bucket >= '2026-11-01' GROUP BY h, service;

REFRESH ROLLUP logs_5m;          -- now, without waiting for the background job
REFRESH ROLLUP logs_5m FULL;     -- every bucket, for rows later than the lateness
REFRESH ROLLUP logs_5m REBUILD;  -- fold every bucket again, after an UPDATE
DROP ROLLUP IF EXISTS logs_5m;
```

- **The bucket** is `date_bin('<width>', field)`, any whole number of seconds, minutes, hours, days or weeks counted from 1970-01-01, or `date_trunc('second' | 'minute' | 'hour' | 'day', field)`, of a `TIMESTAMP` column. The groups are `KEYWORD`, `INT`, `BOOL` or `TIMESTAMP` columns, and the aggregates `COUNT(*)`, `COUNT(f)` (the rows whose `f` is not NULL), and `SUM(f)`, `MIN(f)`, `MAX(f)` and `AVG(f)` of numeric columns (`MIN` and `MAX` of a `TIMESTAMP` too). A `WHERE` keeps some rows only; `FROM`, when written, names the table after `ON`.
- **The rollup is an ordinary table**, named by the statement, with a row per bucket and group: `rollup_key` (its primary key, a number made of the bucket and the group's place in it), the bucket (`bucket`, or the name `AS` gives it), the group columns, typed as in the table, and a column per aggregate, `count`, `count_<field>`, `sum_<field>`, `min_<field>` and `max_<field>` unless `AS` names it. `AVG(f)` keeps `sum_f` and `count_f`, so the mean is `SUM(sum_f) / SUM(count_f)`, which skips NULLs as `AVG(f)` does (a rollup made before 0.3.4 keeps `sum_f` and `count` only, whose quotient counts the rows without a value too; make it again for the other). A `count` column (`COUNT(*)`) is always kept. A rollup that counts a field's values is marked `needs_chloedb_0_3_4` in its definition, which 0.3.3 refuses by name rather than refresh it wrong. Its columns are kept in the columns only, not again as whole rows (`stored = false`), which a rollup's queries never need: about 40 bytes a row. Query it by name, and add its rows up into larger buckets or fewer groups (`SUM(count)`, `MIN(min_latency)`); a query of the table is not rewritten onto it. `OPTIONS(retain = ...)` is the rollup's own [retention](#tables), kept by its bucket: a rollup outlives the rows it counts.
- **How it is kept.** `CREATE ROLLUP` folds the table's rows at once, a day of buckets to a query: 20 million lines in about 8 seconds. On a server it answers what it made: the rollup, whether it is `filled`, the buckets folded and the rows written; it leaves the first fold of a table of over a million rows to its refresher, a few seconds on, and until then the rollup has fewer rows (`REFRESH ROLLUP` fills it at once). From then on a refresh compares, bucket by bucket, how many rows of the table the bucket holds with the rollup's count for it (one count by bucket for a day of buckets at a time: every bucket of 20 million lines over 30 days in a quarter of a second), and folds again the buckets that differ, each in one transaction. A plain refresh compares the buckets from the clock less the `lateness` (10 minutes by default; from the newest row, when that is older) to the newest row, and any the rollup has not folded up to now: a row dated in the future is folded, but does not take the window away from the present. `FULL` compares them all, and `REBUILD` folds them all again whatever their counts. Only buckets that hold rows, in the table or in the rollup, are compared, so a row dated in 1970 or 2099 costs a query, not a walk over every bucket between. One refresh of a rollup runs at a time; another waits for it. A server refreshes every rollup every few seconds, and all their buckets every few minutes, each rollup on its own: one whose refresh fails is logged by name, and the others go on. `GET /v1/tables` gives a rollup's table a `rollup` entry with the table it reads, its last good refresh and its last error. An embedding program calls `Db::spawn_rollups` (or runs `REFRESH ROLLUP`; `Db::rollup_health` says how each went). Buckets older than the table's `retain` are not compared again, so they keep what they counted. A bucket of a time so far from 1970 that its key would not fit in 64 bits (a time in nanoseconds stored as microseconds, say) is not folded, and the refresh says so in the log.
- **For tables rows are added to**, such as logs: a change that leaves a bucket's count as it was (an `UPDATE`, or a delete and an insert into one bucket between two refreshes) is never seen by a plain or a `FULL` refresh, only by `REFRESH ROLLUP name REBUILD`.
- **Its table and the one it reads stay as the rollup needs them.** `DROP TABLE` of a table a rollup reads is refused while the rollup is there (`DROP ROLLUP` first, or drop both in one `DROP TABLE logs, logs_5m`); `DROP TABLE` of a rollup's own table drops its definition too. `ALTER TABLE` that drops or renames a column a rollup reads, or that changes a rollup's own table, is refused. The definition is kept as a [stored function](#stored-functions), `rollup__<name>()`, whose body is the statement, so it needs nothing new on disk and travels with the database. On a server with shards, each shard keeps a rollup of its own rows; rollups through a coordinator come later.

## Subqueries, WITH and joins

```sql
-- IN (SELECT ...): rows of the categories that 2001's surveys are in
SELECT id FROM papers WHERE category IN (SELECT category FROM papers WHERE year = 2001 AND kind = 'survey');

-- a scalar subquery
SELECT id, year FROM papers WHERE year > (SELECT AVG(year) FROM papers) ORDER BY year DESC LIMIT 10;

-- WITH: name a query and read it like the table
WITH top AS (SELECT id FROM papers ORDER BY bm25(body, 'graph neural network') DESC LIMIT 50)
SELECT category, COUNT(*) FROM top GROUP BY category;

-- a WITH result as graph seeds
WITH top AS (SELECT id FROM papers ORDER BY bm25(body, 'graph neural network') DESC LIMIT 20)
SELECT id FROM papers WHERE REACHABLE(cites, (SELECT id FROM top), dir => 'in') LIMIT 100;

-- FROM (SELECT ...) and DISTINCT
SELECT category, n FROM (SELECT category, COUNT(*) AS n FROM papers GROUP BY category) AS t WHERE n > 50 ORDER BY n DESC;
SELECT DISTINCT category FROM papers WHERE MATCH(body, 'graph');

-- joins of two tables: on a key, a multi-valued column (each of its values), or three tables
SELECT p.id, v.name FROM papers p JOIN venues v ON p.venue = v.id WHERE v.rank >= 4 AND MATCH(p.body, 'graph');
SELECT a.name, COUNT(*) AS n FROM papers p JOIN authors a ON p.authors = a.id GROUP BY a.name ORDER BY n DESC LIMIT 10;
SELECT p.id, a.name, v.name FROM papers p JOIN authors a ON p.authors = a.id LEFT JOIN venues v ON p.venue = v.id WHERE p.year = 2024;
SELECT id FROM papers WHERE venue IN (SELECT id FROM venues WHERE field = 'ir');

-- joins of the table with itself: along an EDGES field, or on a shared value
SELECT a.id, b.id AS cited FROM papers a JOIN papers b ON a.cites = b.id WHERE a.year >= 2020;
SELECT a.id, b.id AS cited_by FROM papers a LEFT JOIN papers b ON b.cites = a.id WHERE a.id IN ('p1', 'p2');
SELECT b.id, COUNT(*) AS n FROM papers a JOIN papers b ON a.cites = b.id GROUP BY b.id ORDER BY n DESC LIMIT 10;
SELECT a.id, b.id FROM papers a JOIN papers b ON a.category = b.category AND b.id != a.id WHERE a.id = 'p1';
```

Each plans into operators over the single-table queries above ([`EXPLAIN`](#explain) shows them), all run on one snapshot, so every single-table query uses the indexes. Each is bound as it is built, never printed as SQL and parsed again:

- **`IN (SELECT ...)`** and **`NOT IN`** run the subquery first and filter with its values (a multi-valued column contributes each value). **`EXISTS`** is true or false, and a **scalar subquery** must return one column and at most one row. An `EXISTS` or `NOT EXISTS` in `WHERE` whose subquery names the outer row by one equality, `WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer = c.id AND o.total > 100)`, is a semi join (an anti join for `NOT EXISTS`): it runs as `c.id IN (SELECT o.customer FROM orders o WHERE o.total > 100)`, the subquery once, with `NULL`s as `EXISTS` has them (`NOT EXISTS` keeps an outer row whose `c.id` is `NULL`). The subquery may have a `LIMIT`, not groups, aggregates or an `OFFSET`; one that names the outer row anywhere else is an error. A subquery's values reach the outer query's filter as values, so its indexes serve it: `year > (SELECT AVG(year) ...)` is a range of `year`. A scalar subquery that names a column of the outer row is [correlated](#correlated-subqueries-a-ranking-that-reads-other-rows): it ranks, and is not a condition.
- **`WITH`** results and **`FROM (SELECT ...)`** are read like a table. A `WITH` whose rows are rows of the table (no aggregates) keeps their keys, so an outer query over it reads the table restricted to those rows, in the `WITH`'s order unless it orders them itself, and `(SELECT id FROM top)` can seed a graph. The outer query may name any of the table's columns (`SELECT id, category FROM top`), while `*` is the `WITH`'s own columns, as in any SQL. A `score()` or `hops()` the `WITH` selects (`score() AS s`) is read from its rows, by name or by `*`. A column a `WITH` or a `FROM (SELECT ...)` selects as `s.item`, without an alias, is its column `item`, as SQL names it, so the query reading it says `ls.item`; two columns of one name keep their tables' (`s.id`, `p.id`). `WITH RECURSIVE` is not supported: `REACHABLE` and `PATH` walk links.
- **Joins** are `JOIN` (inner), `LEFT JOIN`, `FULL [OUTER] JOIN`, and `RIGHT JOIN` as the first join of a `FROM` (`a RIGHT JOIN b ON ...` runs as `b LEFT JOIN a ON ...`, and `*` keeps `a`'s columns first), of a table with another, with itself or with a `WITH` result; name each side (`papers a JOIN papers b`). `ON` needs an equality between the sides, of two columns of one kind: `KEYWORD`, or `INT` and `FLOAT` compared as numbers (`p.venue = a.id`, an `INT` with a `KEYWORD`, is an error). A multi-valued column joins each of its values. Comparing an `EDGES` field with the key follows its links (`a.cites = b.id`: *b* is a row *a* links to; `b.cites = a.id`: *b* links to *a*); links name keys of their own table, so they join a table with itself. Each `WHERE` or `ON` condition on one side filters that side's scan, and the second side reads only the rows whose join values the first side holds (`b.id IN (...)`, or for links the rows linking to them), before a hash join in memory. Groups of the second side's rows, a `GROUP BY` of its key (and of other columns of its rows) that aggregates only fields of the first side and is ordered by that key among others, are made in the first side's scan, by its join key, before the join (`SELECT p.id, p.title, COUNT(*) AS n FROM reviews r JOIN products p ON r.product_id = p.id GROUP BY p.id, p.title ORDER BY n DESC, p.id`): the join reads a row per group rather than per row. Text, vector and graph functions name one side (`MATCH(a.body, ...)`); `ORDER BY` a score ranks by that side's scan, and when the order is the first side's, scans of a growing number of its rows stop once `LIMIT` rows join. Joined columns of the answer are named by their expression (`b.title`) unless aliased.
- **`FULL JOIN`** gives the rows of a `LEFT JOIN`, then each row of the right side that matched no row of the left, with `NULL`s for the left's columns, in the right's order (`ORDER BY` orders them as any rows): `SELECT coalesce(o.sku, s.sku) AS sku, o.qty, s.qty FROM orders o FULL JOIN stock s ON o.sku = s.sku`. A `NULL` key matches nothing, so its row comes once, from its own side. Since both sides keep their unmatched rows, no condition filters either side's scan: `ON`'s other conditions decide which pairs join (a row they part from every match keeps its `NULL`s), and `WHERE` filters the joined rows, so `WHERE o.sku IS NULL` keeps the stock no order names. Both sides are read whole, without the restriction to the first side's join values or the growing scan of a `LIMIT`. Joins after it join its rows as they join any. It runs on the operators (DataFusion leaves it to them), and through a coordinator, which joins the rows its shards answer; a plan with one needs the plan feature `full_join`.
- Over joined or derived rows, `WHERE`, `GROUP BY`, aggregates, `HAVING`, `ORDER BY`, `DISTINCT` and `LIMIT` also take arithmetic, `||`, `LIKE`, `ILIKE`, `CASE`, `COALESCE`, `LOWER`, `UPPER`, `LENGTH`, `ABS`, `ROUND`, `DOT`, `HASH` and the numeric functions of ranking expressions (`ln`, `pow`, `least`, ...). So does the `SELECT` list over one table (`SELECT id, rating * 2, lower(category), dot(emb, :q) FROM papers WHERE ...`): the table's query finds the rows and the columns are computed in memory, typed as over derived rows. A `SELECT` that also selects a ranking's value (`bm25(..) * 2`, `hops()`) computes its columns as ranking expressions do, as `FLOAT`s. Their conditions follow the same three-valued logic as a single table's, multi-valued columns included (`NOT (b.tags = 'red')` keeps rows without tags). Integer arithmetic is exact, and a result too big for an integer is a `FLOAT`; `/` gives a `FLOAT`, and dividing by zero gives `NULL`. `||` skips `NULL`s, and `LIKE` with a `NULL` side is `NULL`. `ORDER BY` puts rows without a value last in either direction unless `NULLS FIRST`, as over a single table.
- **Engines.** ChloeDB's own operators run these in memory, a row at a time. With the feature `datafusion`, which chloedb-server and the Python package turn on and ChloeCtx leaves off, [DataFusion](https://datafusion.apache.org) runs them on column batches once their single-table queries read more than 4,096 rows; below that the operators answer, faster on so few, reusing the rows DataFusion's attempt already read. Both give the same rows in the same order, but sums of floats can differ in their last digits. On two tables of 100,000 rows, a join with a group took 285 ms on the operators and 60 ms on DataFusion. On two tables of 100 rows it took 0.6 ms on the operators and 2.9 ms on DataFusion; the default engine (`auto`) answered in 1.2 ms. Measured with `chloedb/examples/df_join.rs` on a 4-core cloud Xeon at 2.10 GHz, 2026-11-01. `CHLOEDB_ENGINE` sets the engine:
  - `auto`: the default;
  - `operators`;
  - `datafusion`: DataFusion, whatever the size;
  - `datafusion-only`: a query DataFusion does not run fails, to find what it lacks.

  The operators run what DataFusion does not run yet, and the plan's first line says why. Today that is joins along links, multi-valued columns as sort keys, correlated subqueries, and `maxsim` or `encode` over a subquery.

  chloedb-server runs them on its engine over its own tables and as a [coordinator](server.md#sharding-one-coordinator-many-shards), which runs them above the rows its shards answer for each single-table query: on DataFusion, as Arrow columns from shards of [protocol 10](server.md#grpc).
- On the operators, each single-table query, `WITH` result and side of a join holds at most 1,000,000 rows. On DataFusion they hold as many rows as the query's memory budget allows, and past it sorts and groups spill to disk. `CROSS` joins, and a `RIGHT JOIN` after another join, are not supported.

The executor's plan lines (`--plan`, `/v1/explain`'s `plan`) list each single-table query that ran, with its rows and its own plan.

## Window functions

A window function gives each row a value computed over other rows, its *partition*, without folding them into one as `GROUP BY` does: a row's rank among its category's, the value of the row before, a running total, a moving average.

```sql
-- each paper's rank in its category by citations, and its category's average
SELECT id, category, cites,
       RANK() OVER (PARTITION BY category ORDER BY cites DESC) AS r,
       AVG(cites) OVER (PARTITION BY category) AS avg_cites
FROM papers WHERE year >= 2020;

-- the three most cited of each category: QUALIFY filters by a window
SELECT id, category, cites FROM papers
QUALIFY ROW_NUMBER() OVER (PARTITION BY category ORDER BY cites DESC, id) <= 3;

-- a running total and a moving average of the day before, the day and the day after
SELECT day, clicks,
       SUM(clicks) OVER (ORDER BY day) AS total,
       AVG(clicks) OVER (ORDER BY day ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS smooth,
       clicks - LAG(clicks) OVER (ORDER BY day) AS change
FROM daily ORDER BY day;

-- over groups: categories ranked by size, and each one's share
SELECT category, COUNT(*) AS n, RANK() OVER (ORDER BY COUNT(*) DESC), COUNT(*) / SUM(COUNT(*)) OVER () AS share
FROM papers GROUP BY category;

-- one window, named once
SELECT id, ROW_NUMBER() OVER w, LAG(year) OVER w, SUM(cites) OVER (w ROWS 2 PRECEDING)
FROM papers WINDOW w AS (PARTITION BY category ORDER BY year);
```

- **Functions.** `ROW_NUMBER()`; `RANK()` and `DENSE_RANK()`; `PERCENT_RANK()` and `CUME_DIST()`, `FLOAT`s; `NTILE(n)`, buckets 1 to n as even as they can be, the first ones a row larger (as SQL says, and PostgreSQL and SQLite do); `LAG(x [, offset [, default]])` and `LEAD(..)`, `x` of the row `offset` rows before or after (1 by default), or `default` (`NULL` without one) past the partition's ends; `FIRST_VALUE(x)`, `LAST_VALUE(x)` and `NTH_VALUE(x, n)` of the row's frame; and the aggregates `COUNT(*)`, `COUNT(x)`, `SUM`, `AVG`, `MIN` and `MAX` of the frame. `NTILE`'s, `NTH_VALUE`'s and `LAG`'s numbers are constants. `COUNT(DISTINCT ..)`, `FILTER (WHERE ..)` and `IGNORE NULLS` are errors.
- **`OVER (PARTITION BY .. ORDER BY ..)`.** The partition is the rows with equal values of its expressions (`NULL` equal to `NULL`); without `PARTITION BY` it is every row. The window's `ORDER BY` orders the partition, its `NULL`s last unless `NULLS FIRST` as elsewhere; rows whose order keys are all equal are *peers*, and rows that tie keep the order they came in. Both take expressions of the rows (`PARTITION BY year % 10`, `ORDER BY lower(title)`), or over groups of the groups' values and aggregates.
- **Frames.** `FIRST_VALUE`, `LAST_VALUE`, `NTH_VALUE` and the aggregates read the rows of the row's frame: `ROWS BETWEEN start AND end` counts rows from the row, and `RANGE BETWEEN start AND end` takes the rows whose value of the window's one `ORDER BY` key is within the offsets of the row's (a row without a value has its peers, the other rows without one). The ends are `UNBOUNDED PRECEDING`, `n PRECEDING`, `CURRENT ROW` (in `RANGE`, the row's first or last peer), `n FOLLOWING` and `UNBOUNDED FOLLOWING`, in that order; `ROWS 2 PRECEDING` is `ROWS BETWEEN 2 PRECEDING AND CURRENT ROW`. Without a frame, a window with an `ORDER BY` reads from the partition's first row to the row's last peer, so `SUM(x) OVER (ORDER BY day)` is a running total that counts a day's rows together, and a window without one reads the whole partition. The other functions read the partition whatever the frame says. `GROUPS` frames are errors.
- **Where they go.** A window function is a column of `SELECT`, a key of `ORDER BY`, or part of a condition of **`QUALIFY`**, which filters the rows after the windows are computed and may name a selected window by its alias (`QUALIFY r <= 3`). It is not part of `WHERE`, `GROUP BY` or `HAVING`, which come before, and a window function's argument, partition or order holds no other: compute that one in a subquery in `FROM`. In a grouped query the windows are computed over the groups `HAVING` keeps; `SELECT DISTINCT`, `ORDER BY` and `LIMIT` come after them, so `COUNT(*) OVER () ... LIMIT 5` counts every row.
- **`WINDOW w AS (..)`** names a window that `OVER w` uses as it is, and that `OVER (w ORDER BY ..)` or `OVER (w ROWS ..)` extends with an order or a frame it lacks; a window may name one before it (`WINDOW w AS (PARTITION BY a), v AS (w ORDER BY b)`).
- **Engines.** The operators sort each partition in memory; DataFusion runs windows on column batches, sorting and spilling within the query's memory budget, and both answer alike. The operators run a window DataFusion does not lower yet: a `LAG` or `LEAD` whose default is not a constant of the value's type, `FIRST_VALUE` and `LAST_VALUE` over a `RANGE` frame with an offset, `NTH_VALUE` over a `RANGE` frame that starts after the partition's first row, a `RANGE` offset of another type than its key (`0.5` over an `INT`), and windows over multi-valued columns.

The plan's node is `Window` (`Window: RANK() OVER (PARTITION BY category ORDER BY cites DESC)` in `EXPLAIN`, the functions computed after the rows' own columns), and its JSON needs the plan feature `window`; a version of ChloeDB from before it refuses the plan by that name.

## UNION, INTERSECT and EXCEPT

A set operation takes the rows of two queries together: `UNION` the rows of either, `INTERSECT` those of both, `EXCEPT` those of the first that the second lacks.

```sql
-- the categories of 2024's papers and of the reviews' papers, each once
SELECT category FROM papers WHERE year = 2024 UNION SELECT category FROM reviews;

-- every row of both, repeats kept, ranked together
SELECT id, cites AS n FROM papers WHERE cites > 100
UNION ALL
SELECT id, downloads FROM preprints WHERE downloads > 1000
ORDER BY n DESC LIMIT 10;

-- the venues 2023 had and 2024 lost
SELECT venue FROM papers WHERE year = 2023 EXCEPT SELECT venue FROM papers WHERE year = 2024;

-- a side ordered and cut on its own, in parentheses
(SELECT id FROM papers ORDER BY cites DESC LIMIT 5) UNION ALL (SELECT id FROM preprints ORDER BY downloads DESC LIMIT 5);
```

- **Columns.** Both sides select as many columns, matched by place, and the answer's columns are named as the first side's. Rows are equal when each of their values is, `NULL` equal to `NULL` and an `INT` equal to the `FLOAT` of its value.
- **`ALL`.** Without it each row comes once. With it, `UNION ALL` keeps every row of both sides; `INTERSECT ALL` keeps a row as many times as both sides have it, the fewer of the two counts; `EXCEPT ALL` as many times as the first side has it more than the second.
- **Order.** `INTERSECT` comes before `UNION` and `EXCEPT`, which go left to right, and parentheses group sides: `a UNION b INTERSECT c` is `a UNION (b INTERSECT c)`. `ORDER BY`, `LIMIT` and `OFFSET` at the end apply to the whole and name its columns; a side in parentheses takes its own. Without an `ORDER BY` the rows come in the sides' order: `UNION ALL` the first side's rows, then the second's; `UNION` those rows each where it first comes; `INTERSECT` and `EXCEPT` rows of the first side in its order, the first ones of rows equal to one another.
- **Anywhere a query goes**: a `WITH` result, a derived table `FROM (SELECT .. UNION ..) d`, or a subquery `x IN (SELECT .. UNION ..)`, and each side may group, join, rank, or compute windows. `UNION BY NAME` is an error.
- **Engines.** Both answer alike. The operators run a set operation DataFusion does not lower: one whose sides give a column different types (an `INT` and a `FLOAT`), and `INTERSECT`, `EXCEPT` or `UNION` without `ALL` of a multi-valued column.

The plan's node is `SetOp` (`Union`, `Intersect all` and so on in `EXPLAIN`, the sides below it), and its JSON needs the plan feature `set_ops`; a version of ChloeDB from before it refuses the plan by that name.

## The first row of each group: DISTINCT ON

`SELECT DISTINCT ON (keys) ...` keeps, of the rows with equal keys, the first in the query's order, as PostgreSQL does: a row per group, the groups in the order of their first rows, and `LIMIT` and `OFFSET` count groups. With rows of a long video or document in a table of segments, it answers each video once, by its best segment:

```sql
-- each video's best clip, the 10 best videos
SELECT DISTINCT ON (video) video, id, start_ms, score() FROM clips
WHERE MATCH(transcript, 'penalty kick') ORDER BY bm25(transcript, 'penalty kick') DESC LIMIT 10;

-- each user's latest event
SELECT DISTINCT ON (usr) usr, item, ts FROM events ORDER BY ts DESC, id;

-- the best page of each PDF, by an image query's similarity
SELECT DISTINCT ON (doc) doc, page FROM pages ORDER BY similarity(emb, :q) DESC LIMIT 5;

-- over groups: each country's top seller
SELECT DISTINCT ON (country) country, seller, SUM(price) AS total FROM sales GROUP BY country, seller ORDER BY country, total DESC;
```

- **Keys** are expressions of the rows (`video`, `lower(lang)`, `year % 10`), or columns of the answer by name or number (`DISTINCT ON (2)`), and need not be selected. Keys are equal when their values are: `NULL` with `NULL`, and a multi-valued column's list with the same list.
- **The order.** PostgreSQL requires `ORDER BY` to start with the keys; ChloeDB does not, so `ORDER BY` a score gives the best row of each group, the best groups first. When `ORDER BY` does start with the keys, the answer is PostgreSQL's. Without `ORDER BY`, which row of a group is kept is not defined.
- **Reads.** When the query's order is the scan's (a score, or one numeric column), a `LIMIT` reads the scan in growing slices: four times the rows asked for (at least 32), then four times as many, until that many groups have a row or the scan runs out. A group with many rows near the top costs more reads, never a wrong answer. `DISTINCT ON` one keyword or `INT` field of a table ranked by `bm25` reads less: each segment keeps each group's first row as it scores ([`MAX k PER`](#at-most-k-rows-of-a-value-max-k-per) does the same with k rows), so the scan returns the rows asked for. Ordered by several columns, the rows are sorted in memory, and the scan holds at most 1,000,000 rows.
- **Shards.** A coordinator keeps the first rows over its shards' merged rows, so three shards give what one database does, whether or not the table is placed by the keys (`shard_by`).

The plan's node is `DistinctOn` (`Distinct on: video` in `EXPLAIN`), and its JSON needs the plan features `operators` and `distinct_on`; a version of ChloeDB from before it refuses the plan by that name.

## At most k rows of a value: `MAX k PER`

`ORDER BY ... MAX k PER field` keeps, of the rows with equal values of `field` (a `KEYWORD` or a number of one value: a `TEXT` field is an error, as rows rarely share a whole text), the first `k` in the query's order, and the rows kept stay in that order: a ranking with at most two clips of a video, three products of a brand or two arguments of a debate. `LIMIT` and `OFFSET` count the rows kept.

```sql
-- the 20 best clips, at most two of a video
SELECT id, video, score() FROM clips ORDER BY bm25(transcript, :q) DESC MAX 2 PER video LIMIT 20;

-- the newest products in stock, at most three of a brand
SELECT id, brand, title FROM products WHERE in_stock ORDER BY added DESC, id MAX 3 PER brand LIMIT 30;

-- in a subquery: of each video's two most viewed clips, those about a dog
SELECT id FROM clips WHERE MATCH(transcript, 'dog')
  AND id IN (SELECT id FROM clips ORDER BY views DESC, id MAX 2 PER video LIMIT 1000);
```

- **It ends `ORDER BY`**, and needs one: the rows a value keeps are its first in that order. `k` is a whole number, 1 or more, and `MAX 1 PER x` is `DISTINCT ON (x)`, with the same plan.
- **A field of one value.** `field` is a field of the rows (`video`, `t.brand` in a join), and `NULL` is one value of its own. A multi-valued field is refused, since a row with several values would be in several groups.
- **Not with `DISTINCT`** or `DISTINCT ON`, which already keep one row of each.
- **Reads.** A table's own rows ranked by `bm25` and capped by a keyword or `INT` field keep the cap as they are scored: each segment keeps each value's best k rows (and the rows tied with its k-th) and skips the rows that can no longer reach the `LIMIT`, so the query reads its rows once. Other rankings, and caps over joined or derived rows or by other fields, read growing slices of the ranking until `LIMIT` rows are kept or it runs out, as [`DISTINCT ON`](#the-first-row-of-each-group-distinct-on) does. A field whose values cannot fill the `LIMIT`, such as one row of each of 15 topics for a `LIMIT` of 20, reads every row that matches either way, and returns 15.
- **Shards.** A coordinator keeps each value's first rows over its shards' merged rows, so three shards give what one database does.

The plan's node is `DistinctOn` with `per` (`At most 2 per: video` in `EXPLAIN`). With `k` above 1 its JSON needs the plan feature `per_cap`, so a version of ChloeDB from before it refuses the plan rather than keeping one row of each value. Python's builder writes it `.max_per(2, "video")`.

On ArguAna (BEIR's 8,674 arguments from 592 debates on 15 topics; a document's debate and topic are in its id), every one of the 1,406 queries ranked by `bm25(body, :q)`, its own argument left out as BEIR scores it, top 20. Times are the median over queries of each query's median of 3 runs, one thread on a 4-core cloud VM (Intel Xeon @ 2.10GHz):

| top 20 | nDCG@10 | debates | topics | p50 ms | p99 ms |
|---|---:|---:|---:|---:|---:|
| uncapped | 0.489 | 7.5 | 3.8 | 1.6 | 4.1 |
| `MAX 1 PER debate` | 0.307 | 20.0 | 7.2 | 2.4 | 5.4 |
| `MAX 2 PER debate` | 0.379 | 14.5 | 5.9 | 2.3 | 5.0 |
| `MAX 3 PER debate` | 0.419 | 12.6 | 5.4 | 2.3 | 5.5 |
| `MAX 1 PER topic` | 0.301 | 14.9 | 15.0 | 2.8 | 5.7 |
| `MAX 2 PER topic` | 0.376 | 16.3 | 11.4 | 2.6 | 5.4 |
| `MAX 3 PER topic` | 0.416 | 14.2 | 9.1 | 2.4 | 5.4 |

Debates and topics are the mean number of each in a query's answer. Each segment keeps the cap as it scores, so a cap costs about 1.5 times the uncapped query whatever the field: BM25 can skip fewer rows, since the 20th row kept scores lower than the 20th of all. One row of each of the 15 topics cannot fill 20 rows, so that query scores every argument that matches, once, and returns 15. Before the scan kept the cap, the same queries read growing slices of the ranking: 6.0 to 6.1 ms by debate, 7.1 and 18.8 ms for three and two per topic, and 549 ms for one per topic, which read every row again at each slice (with the uncapped query at 3.3 ms in that run).

Caps trade relevance for spread. ArguAna's answer to a query is the argument against it, in the query's own debate, so a cap on debates also caps how many of the right debate's arguments the ranking can show: nDCG@10 falls from 0.489 to 0.307 with one per debate, and to 0.419 with three. The run is `chloedb-per-cap-bench` ([`results/text/per_cap.json`](https://github.com/chloetech-ai/chloedb/blob/main/chloedb/results/text/per_cap.json)).

## As much as fits: `PACK n TOKENS`

**Experimental:** what `PACK` reads and how it chooses may change between releases. Against the top rows of a ranking cut in the client ([measured](https://github.com/chloetech-ai/chloedb/blob/main/chloedb/results/pack/README.md) on SciFact and NFCorpus), it reaches more of the relevant documents when the budget holds one to three (SciFact 0.74 against 0.65 at 500 tokens) and spends less of the budget on them, and the two are the same once it holds five or more.

A query's answer is often the context a language model is given, and the model has a budget of tokens, not of rows. `PACK n TOKENS` at the end of a SELECT fills that budget: it reads the query's rows in its order and decides each once, so that a long row, a row that repeats one already taken and a row linked to one taken are weighed inside the query rather than by the application after a top k.

```sql
-- what memory says about the task, in at most 4,000 tokens
SELECT id, kind, text FROM memory
WHERE superseded = false
ORDER BY rrf(bm25(text, :task), similarity(emb, :q)) DESC
PACK 4000 TOKENS DEDUPE 0.85 FOLLOW links 1 HOP WITH RECEIPT;
```

Each row, in the query's order, is:

- **taken** whole when its text fits what is left of the budget;
- **excerpted** when it is longer than one row may be (`EXCERPT n`; a quarter of the budget by default) or than what is left: the stretch of its longest text column with the most terms of the query's `bm25` and `MATCH` texts (or its start, when it has none), marked `… ` where it was cut. Terms are found as the column's analyzer makes them (stemmed for `english`, dictionary words for Chinese and Japanese under `standard`), and the text is cut between words, or inside a run without spaces (Chinese, Japanese, Thai, a URL, base64), so an excerpt never spends more than its room;
- **skipped as a near-duplicate** of a row taken before it, with `DEDUPE x`: at least the share x of the shorter one's terms (as its analyzer makes them) are in the other, or, with `DEDUPE x ON emb`, the cosine of their vectors is at least x;
- **skipped as over budget** when not even an excerpt of 32 tokens fits;
- **skipped as no text** when none of its counted columns holds text.

With `FOLLOW edges n HOP` (1 to 3 hops, 1 by default), each row taken brings the rows its [`EDGES`](#graphs) field reaches, up to 16, right after it in the order of its edges, if they pass the query's `WHERE`, decided the same way (`linked from k`). The rows are read by key, a hop at a time, so `FOLLOW` works through a coordinator, where graph walks do not: a decision a file note links to is taken with the note, though it shares no word with the task.

The answer is the rows taken, in that order, with two more columns: `pack_tokens`, the tokens the row spends, and `pack_decision` (`take`, `excerpt`, `linked from k`). `WITH RECEIPT` adds the rows skipped (`skip: near-duplicate of k`, `skip: over budget`, `skip: no text`), with their text columns `NULL`, so that the text of the rows answered is still what fits the budget, and the columns `pack_score`, the query's score of the row, and `pack_used`, the tokens spent up to and with it. In Rust, `Db::pack` answers the rows and a receipt with every decision and the snapshot read (`txn` and `seq_no`).

- **The budget** is a whole number from 1 to 10,000,000, or a parameter that holds one: `PACK :budget TOKENS`.
- **The text counted** is every column a row read holds text in, but the key's, or the columns `OF a, b` names. Tokens are an estimate (`chloedb::query::pack::count_tokens`), as the real count depends on the model's tokenizer: a run of Latin letters is a token per six letters, a run of another alphabet's (Cyrillic, Greek, Arabic, Hebrew, the scripts of India) a token per three, a run of digits a token per three, a CJK or Thai character or a punctuation mark one each. Against OpenAI's cl100k_base and o200k_base it counts English and code 13 to 27% high, the safe side; other scripts are counted high against o200k_base but low against cl100k_base, by up to 2.4 times for Hindi ([measured](https://github.com/chloetech-ai/chloedb/blob/main/chloedb/results/pack/README.md)).
- **Reads.** The rows are read in growing reads, the first 64, then the first 320, 1,344 and so on, each from `OFFSET`, and the rows seen before are passed over by their keys; reading stops once what is left of the budget is too small for any row, so a query whose first rows fill the budget reads once. A deeper read rather than the next page, because a fused ranking (`rrf`, a weighted sum) ranks its candidates to a depth that grows with the `LIMIT`: a deeper read is a better ranking of the same rows, and one that comes back short is the end of the ranking. (In a table without a primary key the reads are pages instead.) `LIMIT` is the most candidates read (1,000 without one), and `OFFSET` where they start. Every read, and every row followed, is a query of its own on the same snapshot: on a server, a lease held for the pack (kept for the client with `hold`, or the client's own `snapshot`), and the statement is admitted once, by its text, and held to one deadline. Through a coordinator each shard reads its latest version at each read.
- **Choosing** is greedy in the query's order. A row that does not fit whole is excerpted rather than skipped, so that one long log cannot spend the budget, and a smaller row after one over the budget is still taken.
- **Where.** On a file, a server and a coordinator alike: a coordinator merges each read from its shards, and packs. A key limited to tables runs it as it runs the query without the clause. `PACK` inside a subquery is not the clause (`pack` stays a name there, and anywhere it is not followed by a number and `TOKENS`).

## Graphs

Rows with an `EDGES` field are nodes of a graph. Graph predicates and scores take **seeds**: a key (`'p1'`), a parameter, a tuple of keys (`('p1', 'p2')`), or a subquery, whose filters, ranking and `LIMIT` are those of any query. A subquery over the same table seeds with its rows; one over another table returns one column, whose values are the keys to start from, such as the items a user's recent events name: `ppr(cooc, (SELECT item FROM events WHERE usr = :u ORDER BY ts DESC LIMIT 20))`. With `weighted => 1`, a seed's restart weight is its row's score, summed over the rows that name it: `ppr(cooc, (SELECT item FROM events WHERE usr = :u ORDER BY w DESC), weighted => 1)` starts from each item in proportion to the sum of its events' `w`, and ordering the events by `decay(ts, ...)` instead weighs recent ones more.

```sql
-- papers within two citations of p1, either way, nearest first
SELECT id, title, hops() FROM papers
WHERE REACHABLE(cites, 'p1', hops => 2, dir => 'both') ORDER BY hops(), id LIMIT 20;

-- one shortest path, only through surveys
SELECT id, hops() FROM papers
WHERE PATH(cites, 'p1', 'p9', max_hops => 6, through => kind = 'survey') ORDER BY hops();

-- vector search as the way in: what the two nearest papers cite, by similarity
SELECT id FROM papers
WHERE REACHABLE(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT 2))
  AND NOT REACHABLE(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT 2), hops => 0, min_hops => 0)
ORDER BY similarity(emb, :q) DESC LIMIT 20;

-- personalized PageRank from the same seeds, fused with BM25
SELECT id FROM papers
ORDER BY rrf(bm25(body, :t), ppr(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT 5))) DESC LIMIT 20;
```

| function | what it is | options (defaults) |
|---|---|---|
| `REACHABLE(f, seeds, …)` | rows *v* with `min_hops` ≤ *d*(seeds, *v*) ≤ `hops` | `hops 1`, `min_hops 1`, `dir 'out'` (or `'in'`, `'both'`), `label 'a,b'`, `through <predicate>`, `max_nodes 100000` |
| `PATH(f, from, to, …)` | the rows of one shortest (unweighted) path | `max_hops 6`, `dir`, `label`, `through`, `max_nodes` |
| `hops()` | a row's distance from the seeds, or its position on the path; `ORDER BY hops()` is nearest first | |
| `ppr(f, seeds, …)` | personalized PageRank, usable wherever a score is, including `rrf` and weighted sums | `alpha 0.15`, `eps 1e-4` (a row is pushed while its residual is at least `eps` times its number of edges, so scaling every weight alike changes nothing), `dir 'both'`, `label`, `max_pushes` (a walk cut there says so: `max_pushes_reached` in the stats), `weighted 0` (`1`: the seed subquery's scores are the restart weights) |
| a `VECTOR` field as `f` | `REACHABLE` and `PATH` follow similarity edges: a row's neighbours are its `k` most similar rows | `k 10`, `min_sim` |

Graph queries run on one database: a [coordinator](server.md#sharding-one-coordinator-many-shards) refuses them, since a row's edges point at rows other shards hold ([graphs on a cluster](graphs.md#on-a-cluster)).

The [query console](server.md#the-query-console)'s Graph tab builds these queries (seeds from keys, a search or a condition; links or similarity; neighbourhood, path or PageRank) and draws the answer.

`through` restricts the rows a traversal may pass through (seeds and end points are exempt); the rest of `WHERE` filters the rows it ends at. `max_nodes` bounds the rows a traversal meets besides its seeds (and a path's ends): a `REACHABLE` that would meet more, or a `PATH` that meets more before it finds a path, is an error saying so, never a part of the answer. `max_hops => 0` is a path of no hops (a seed that is also a target); `max_nodes` and `k` are 1 or more. `REACHABLE(f, seeds, hops => 0, min_hops => 0)` is the seed set itself, so `NOT` of it removes the seeds from a result. `hops()` refers to the first `REACHABLE` or `PATH` in `WHERE`. The [Graphs](graphs.md) page explains how edges are stored and what these queries cost.

Edges are written as part of their row: a JSON array of target keys, each optionally with a label and a positive weight.

```text
["p2", ["cites", "p7"], ["extends", "p9", 0.5], {"to": "p11", "label": "refutes"}]
```

### Edge tables

An `EDGES` field belongs to its source row, so adding one edge rewrites the row, its text and its vectors. A graph kept up as events arrive, such as items seen together, goes in an **edge table** instead: a table whose rows are edges, declared with `OPTIONS(edges = 'src, dst, w')`. A walk over the table whose keys it holds names the edge table where it would name a field:

```sql
CREATE TABLE cooc (
  id KEYWORD PRIMARY KEY,                  -- 'src>dst'
  src KEYWORD NOT NULL REFERENCES items,
  dst KEYWORD NOT NULL REFERENCES items,
  w FLOAT DEFAULT 0
) OPTIONS(edges = 'src, dst, w');

SELECT id FROM items
ORDER BY ppr(cooc, (SELECT item FROM events WHERE usr = :u ORDER BY ts DESC LIMIT 20)) DESC LIMIT 200;
```

- Each row is an edge from the row whose key is in `src` to the row whose key is in `dst`, weighted by `w`, or by 1 without a weight column (`edges = 'src, dst'`). `src` and `dst` are stored, single-valued `KEYWORD` or `INT` columns of the type of the other table's key, and the weight is a stored `FLOAT` or `INT`. If `src` references a table, walks are over that table.
- Writing an edge writes its row and nothing else: insert it, delete it, or add to its weight with an `add` operation, whose `doc` creates the row on the edge's first event ([writes](server.md#writes)).
- Edges are directed, from `src` to `dst`. For a click on item *i* after the user's last items *h*₁ … *h*₅ (*h*₁ the most recent), adding 1/*g* to the edge *h*_g → *i* records what comes after what: `ppr(cooc, :x, dir => 'out')` and `REACHABLE(cooc, :x, dir => 'out')` follow the items that came after *x*, and `dir => 'both'`, `ppr`'s default, reads them as seen together. That is five `add` operations per event. For the next item alone, counts of what came after the last one or two items are cheaper and exact ([Next items](next-items.md)).
- An edge is followed while its weight, as it is now, is above 0. `UPDATE cooc SET w = w * 0.9` decays every weight in place, and an edge whose weight is 0 or `NULL` stays in the table without being followed. `src` and `dst` are never changed in place: an `UPDATE` of a `KEYWORD` one rewrites the row, and `set`, `add` or an `UPDATE` of an `INT` one is refused, as for `shard_by` (delete the row and insert it again).
- `REACHABLE`, `PATH`, `hops()` and `ppr` take an edge table with every option but `label`, since its edges have none. A walk over `cooc` and a walk over an `EDGES` field holding the same edges and weights find the same rows at the same hops and the same PageRank, up to rounding.
- Each segment of the edge table indexes its rows by the keys they link, which flushes and merges build, so a walk finds a row's edges with one lookup of its key in each of the edge table's segments ([how, and what it costs](graphs.md#edge-tables)).

## Stored functions

A function is kept in the database under a name, and any query calls it as it calls a built-in one. Its body is SQL: an expression of its parameters for a function that returns a value, a `SELECT` for one that returns rows (`RETURNS TABLE`).

```sql
CREATE FUNCTION boosted(s FLOAT, importance FLOAT, w FLOAT DEFAULT 0.2)
  RETURNS FLOAT AS :s * (1 + :w * :importance);

SELECT id FROM memories WHERE MATCH(body, :q)
ORDER BY boosted(bm25(body, :q), importance) DESC LIMIT 10;

CREATE FUNCTION recall(q TEXT, k INT DEFAULT 10) RETURNS TABLE AS
  SELECT id, body FROM memories WHERE MATCH(body, :q)
  ORDER BY bm25(body, :q) DESC LIMIT :k;

SELECT * FROM recall('tokio runtime', k => 5);
SELECT r.id, m.created FROM recall(:q) AS r JOIN memories m ON m.id = r.id;

CREATE OR REPLACE FUNCTION boosted(...) RETURNS FLOAT AS ...;   -- version 2
SELECT "boosted@1"(bm25(body, :q), importance) FROM memories ...;  -- pins version 1
CREATE OR REPLACE FUNCTION boosted FROM VERSION 1;             -- a rollback: version 3, as 1
DROP FUNCTION IF EXISTS boosted;                               -- a tombstone: version 4
DROP FUNCTION boosted VERSION 2;                               -- deletes an old version
```

- **The definition**: `CREATE [OR REPLACE] FUNCTION name(parameter TYPE [DEFAULT value], ...) RETURNS TYPE | RETURNS TABLE [LANGUAGE sql] [OPTIONS (description = '...')] AS body`, the body also between `$$` and `$$`. Types are `INT`, `FLOAT`, `TEXT`, `KEYWORD`, `BOOL`, `TIMESTAMP`, `JSON`, `VECTOR` or `VECTOR(n)`, and `KEYWORD[]` and `INT[]`; a default is a number, a quoted string, `TRUE`, `FALSE` or `NULL`, and the parameters after one with a default have one too. In the body a parameter is `:name`, as in a [rank profile](#rank-profiles-a-ranking-kept-with-its-table), so the body runs as written as a query with the same parameters. A body that returns a value reads only its parameters: a column is passed in as an argument. Names are letters, digits and `_`, in any case; the name of a built-in function (`lower`, `bm25`, `coalesce`, ...) is refused, and there is one function of a name, whatever its parameters. `CREATE` checks that a `RETURNS TABLE` body plans against the tables, its parameters given their defaults or a value of their type.
- **A call is written out**: binding replaces each call with its body, the arguments in place of the parameters, so the query plans and ranks as if it were written out (indexes, `score()` and a shard's pushdown included), and `EXPLAIN` begins with `calls stored function boosted@2`. Arguments are by position, then by name (`w => 0.5`); a missing one takes its default. A literal argument of another type than its parameter's is refused; any other expression is computed as the body would with it, and the value is the body's (`RETURNS FLOAT` of `:a + :b` with two `INT`s is an `INT`). A function that returns rows is called in `FROM`, alone, joined, or in `WITH`, and its arguments are values or parameters of the query, not columns. A function may call others, up to 16 deep.
- **Versions never change**: `CREATE` writes version 1; `CREATE OR REPLACE` the next version, or nothing when the definition is the current one's; a new version returns what the old one did (a value, or rows). `DROP FUNCTION` writes a tombstone version, so version numbers keep rising and `"f@2"` always means one body. A query calls the current version, the highest that is not a tombstone, or pins one with its name and version in double quotes, which still runs after a drop. `CREATE OR REPLACE FUNCTION f FROM VERSION n` makes a new version with version *n*'s definition; `DROP FUNCTION f VERSION n` deletes an old version (not the current one).
- **Kept as rows**: each version is a row of the system table `_functions` (`id` `name@version`, `name`, `version`, `language`, `kind`, `signature`, `body`, `hash`, `description`, `created`, `created_by`, `dropped`), written by its own transaction, so it is logged, numbered and shipped to replicas as writes are, and checkpoints and copies carry it. `SELECT * FROM _functions` reads it; it is not one of the database's tables, and only `CREATE` and `DROP FUNCTION` write it. The first function adds it, and the database then needs the format 3 feature `functions`, which a version of ChloeDB from before stored functions refuses by name. A database from before field ids keeps none (upgrade it first).
- **From Rust**, `Db::sql` runs both statements, `Db::define_function` runs a parsed one and records who wrote it, and `Db::functions` lists every version.
- **On chloedb-server**, `CREATE` and `DROP FUNCTION` need an admin key, whose name is the version's `created_by`; any key that may read calls them. `GET /v1/functions` lists each function with its current version and every version's definition.
- **Through a coordinator**, a version has one number and one body on every shard: `CREATE` and `DROP FUNCTION` ask every replica which versions it holds, number the new one past all of them, and write it on each shard, so they wait for every replica to answer. A coordinator calls the versions every replica holds alike (a new version once every shard has it) and stops calling a dropped function as soon as any replica holds the drop. A shard added to the cluster gets every version under its number. Each shard records the coordinator's credential as `created_by`.

## Language models: `llm()`

`llm(model, instructions, input, ...)` is a language model's answer to the instructions about the inputs, for each row a query returns. A stored function keeps the prompt and the model under a name, versioned as any function is: a new prompt or model is a new version, `"summarize@1"(body)` still asks the old one, and `FROM VERSION n` rolls back.

```sql
CREATE FUNCTION summarize(t TEXT) RETURNS TEXT
AS llm('anthropic/claude-haiku-5-5', 'Summarize this review in one sentence.', :t);

CREATE FUNCTION mood(t TEXT) RETURNS KEYWORD
AS llm('anthropic/claude-haiku-5-5', 'How does this review feel?', :t, choices => 'positive,negative,mixed');

SELECT id, title, summarize(body), mood(body) AS mood
FROM reviews WHERE MATCH(body, :q) ORDER BY bm25(body, :q) DESC LIMIT 10;

-- the stored summary where there is one, else the model's
SELECT id, coalesce(summary, summarize(body)) FROM reviews ORDER BY rating DESC LIMIT 20;

-- BM25's top 50, ranked by a grade the model gives each
WITH top AS (SELECT * FROM reviews WHERE MATCH(body, :q) ORDER BY bm25(body, :q) DESC LIMIT 50)
SELECT id, title, score() FROM top
ORDER BY llm('anthropic/claude-haiku-5-5', 'Grade from 0 to 10 how well the review answers the question.',
             'Question: ' || :q, body, returns => 'number') DESC LIMIT 10;
```

- **Where it runs.** A query that calls `llm()` runs in two steps: the query without the calls finds its rows (its `WHERE`, `ORDER BY` and `LIMIT` as written), computing what the calls read of each row, and then the model is asked about those rows, eight at once, and the columns are computed of the answers. A call may be part of a column, in arithmetic, `||`, comparisons, `CASE` and functions of values; a `CASE` asks only for the branch it takes, and `coalesce` only where the values before the call are `NULL`. `llm()` is a column of `SELECT` or part of `ORDER BY`; anywhere else (`WHERE`, `GROUP BY`, `HAVING`, a join, a `WITH`, a subquery, `UNION`) it is refused, as it would run for rows the query reads rather than returns.
- **In `ORDER BY`** the calls rank the rows the query finds without that `ORDER BY` and its `LIMIT`: the top rows of a first ranking, as a [rerank phase](#rerank-phases) ranks them. Rows that tie keep the first ranking's order, a `NULL` ranks last, `LIMIT` and `OFFSET` cut the ranked rows, and `score()` is the first key's value.
- **Arguments.** `model` is a string: `anthropic/<model>` (a Claude model, its key in `ANTHROPIC_API_KEY`), `openai/<chat model>`, or the name of a model directory whose `endpoint.json` names a chat server (`"api": "chat"` for OpenAI's `/chat/completions`, as vLLM, Ollama and LiteLLM serve it, or `"api": "anthropic"`), as for [text made of text](server.md#text-from-text). A directory's name is letters, digits, `_`, `-` and `.` (two such names joined by `/` at most): an absolute path, `..` or a backslash is refused, so a query reads no `endpoint.json` outside the models directory, and an `endpoint.json`'s `key_env` names a provider's key variable or a `CHLOEDB_MODEL_KEY_*` one. A query or a row cannot choose the model. The instructions are the system prompt, a string or a parameter; each input is a part of the user's message, apart by a blank line, and a `NULL` input is left out. A call with no input is `NULL`, and asks nothing.
- **Answers** are text, trimmed. `choices => 'a,b,c'` asks for one of them and gives it as written there (the answer in any case, or the one choice an answer that says more names), else `NULL`. `returns => 'number'` gives the first number of the answer as a `FLOAT`, and `returns => 'bool'` its yes or no as a `BOOL`, else `NULL`. `max_tokens => n` bounds the answer (1024 for text, 64 otherwise). The model answers with its own sampling defaults: no temperature is sent (current Claude models refuse one).
- **Cost.** Each distinct question (model, prompt and input) is asked once, and its answer kept in memory (10,000 of them), so the same query again asks nothing. A query asks about 100 rows at most, and a process makes 600 requests a minute at most for `llm()`: past either, the query fails before asking (chloedb-server's `--llm-rows` and `--llm-per-minute`, `chloedb::query::llm::set_limits` from Rust). A failed request fails the query (503), after the endpoint's retries. The answer's plan lines end with one that says how many rows were asked about, how many requests were made, and how many answers came from memory; `EXPLAIN` shows the plan of the rows.
- **Inputs are data.** A row's text is the user's message and the instructions the system prompt, and the model has no tools, so a row's text can change that row's answer and nothing else.
- **A coordinator** asks the model about the merged rows, on the coordinator, so no shard asks about a row the merge cuts, and its shards need no key. A body that calls `llm()` is a function that returns a value: `CREATE` refuses one in a `RETURNS TABLE` body, and checks each call's model and options. chloedb-server counts the requests in `chloedb_llm_requests_total`, `chloedb_llm_cached_total` and `chloedb_llm_failures_total`.

## Classifiers: `CREATE CLASSIFIER` and `classify()`

> **Experimental.** This is new in 0.3.0, and its options and answers may change in a later release.

A classifier learns a table's labels from its rows' embeddings, and is kept as a [stored function](#stored-functions) of a vector: versioned, rolled back and dropped as any function is, and held by every shard of a cluster. It classifies rows by their stored vectors, and a query's text by the same model's vector of it.

```sql
-- trains on the rows with a label; answers what it measured
CREATE CLASSIFIER topic ON products(emb) LABELS category
  OPTIONS (method = 'logistic', holdout = 0.2) WHERE in_stock;

-- each row's class, a column of the rows a query returns
SELECT id, title, topic(emb) AS topic FROM products WHERE MATCH(title, :q) LIMIT 20;

-- a query's class: :v is the query's vector ({"embed": "red leather sofa", "field": "emb", "table": "products"})
SELECT topic(:v), topic(:v, 'score'), topic(:v, 'labels');
SELECT id FROM products WHERE MATCH(body, :q)
ORDER BY bm25(body, :q) * (1 + 0.25 * CASE WHEN category = topic(:v) THEN 1 ELSE 0 END) DESC LIMIT 10;

CREATE OR REPLACE CLASSIFIER topic ON products(emb) LABELS category;   -- version 2
CREATE OR REPLACE FUNCTION topic FROM VERSION 1;                       -- back to the first
DROP CLASSIFIER topic;
```

- **Training.** `CREATE CLASSIFIER name ON table(vector_field) LABELS column [OPTIONS (...)] [WHERE condition]` reads `SELECT vector_field, column FROM table WHERE column IS NOT NULL [AND condition]`, at most `max_rows` rows (200,000), keeps the labels with at least `min_rows` rows (2), and holds out a share of them (`holdout`, 0.2, chosen by a hash of each vector, so the same rows every time) to measure the model it keeps. `method = 'logistic'` (the default) fits softmax regression (Adam, `epochs` 30, `learning_rate` 0.01, `l2` 0.0001); `method = 'centroid'` takes each label's mean vector, in one pass. The labels are the values of a `KEYWORD`, `TEXT`, `INT` or `BOOL` column, and the field a `VECTOR` field of one vector a row; a classifier has 10,000 labels at most. The statement answers one row: `labels`, `rows`, `train_accuracy`, `holdout_rows`, `holdout_accuracy`, `took_ms`. A coordinator reads the rows from its shards, trains, and writes the function on every shard.
- **The function** is `name(v VECTOR, returns TEXT DEFAULT 'label') RETURNS KEYWORD`, its body a call of `classify(:v, model, returns => :returns)`, `model` the classifier as text: its labels, a row of weights a label quantized to 8 bits with a scale of its own, and a bias (about a kilobyte a label for a 512-number vector), and the table, field and embedding model it was trained on. `name(v)` is the label with the highest probability (the softmax of the scores), `NULL` for a `NULL` vector or one below the classifier's `min_score` option (0); `name(v, 'score')` that probability; `name(v, 'labels')` the best label and those whose probability is at least `min_score` and a tenth, best first.
- **Where it runs.** A call of a value of no row, a parameter's vector, is computed before the query is planned, so it stands anywhere: `WHERE`, `ORDER BY`, a join (its `'labels'`, a list, is a column of the rows only). A call of a row's vector is a column of the rows a query returns, or ranks them in `ORDER BY`, as `llm()` is: computed after the query finds them, for 100,000 rows at most. To filter or group rows by their class, keep it in a column: a `KEYWORD` field `OPTIONS (derive = 'title', model = 'classifier:topic')` holds the label of each row written ([keywords made of text](server.md#keywords-made-of-text-languages-keyphrases-near-duplicates-entities)).
- **Text.** `Db::classify(name, text, k)` (Rust) and `POST /v1/classify {"name", "text", "k"}` embed a text with the model of the field the classifier was trained on and give its `k` best labels with their probabilities.
- **Measured** on Wayfair's WANDS (`chloedb-classify-eval`, `chloedb/results/text/classify_wands.json`, an Intel Xeon at 2.10 GHz, 4 cores): 42,994 products, `emb` potion-retrieval-32M's vector of a product's name, 501 product classes. The logistic classifier names the class of 81.9% of held-out products (centroid 81.3%) and the class of 62.9% of the 480 queries (centroid 51.0%; among its best three, 80.4%), at 0.07 ms a query's text, embedding included; it trains in 31 s (centroid 2 s), and a row's class costs 40 µs. As a boost, `bm25(body, :q) * (1 + 0.1 * CASE WHEN class = topic(:v) THEN 1 ELSE 0 END)` raised nDCG@10 from 0.433 to 0.439 (p = 0.002, the same on the odd-numbered queries not used to choose the weight), and a weight of 0.5 lowered it: a wrong class costs the rows that are right.

## Recommenders: `CREATE RECOMMENDER` and `recommend()`

A recommender keeps counts of a table of events (what came right after what, what was seen together, how often each item was seen, each user's last items) as the events are written, and `recommend(name, user)` ranks items for a user by them, fused with the similarity of the user's recent items and with popularity, leaving out the items the user had.

```sql
CREATE TABLE events (id KEYWORD PRIMARY KEY, usr KEYWORD, item KEYWORD, ts TIMESTAMP);
CREATE RECOMMENDER shop ON events(usr, item, ts) ITEMS items(emb) OPTIONS (window = 3, history = 20);

-- items for a user, as a score: filter, join and fuse it as any other
SELECT id, title FROM items WHERE in_stock ORDER BY recommend('shop', :u) DESC LIMIT 10;
SELECT id FROM items WHERE MATCH(title, :q) ORDER BY rrf(bm25(title, :q), recommend('shop', :u)) DESC LIMIT 10;

-- the items themselves, and which list each came from (its rank there)
SELECT * FROM recommend('shop', 'u42', k => 5);
-- item | score  | next | co | similar | popular
-- 'p9' | 0.0485 | 1    | 2  | 7       | NULL

DROP RECOMMENDER shop;
```

- **The definition.** `CREATE [OR REPLACE] RECOMMENDER name ON events(user, item, time) [ITEMS items[(vector_field)]] [OPTIONS (window = 3, history = 20)]`. The user and the item are stored, single-valued `KEYWORD` or `INT` columns of the events table, and the time a `TIMESTAMP`, `INT` or `FLOAT` column (a `NULL` time places the event last). `ITEMS` names the table whose keys the items are (of the item column's type), and its `VECTOR` field, if any, for the similar items. `window` (1 to 50) is how many events apart two items still count as seen together, and `history` (1 to 1,000) how many of a user's last events are kept, recommended from and left out of the answer. `OR REPLACE` counts the events again.
- **What it keeps.** The statement counts the events already in the table, in one transaction, and answers one row: `recommender`, `counts`, `events`, `users`, `pairs`, `took_ms`. The counts are rows of an ordinary table, `<name>_counts` (`id`, `kind`, `src`, `dst`, `next`, `co`, `n`, `recent`), which any query reads: a `pair` row's `next` is how often `dst` came right after `src` in one user's events, and its `co` how often the two came within `window` events of each other (both ways); an `item` row's `n` its events; a `user` row's `recent` the user's last `history` events as JSON. The definition is a [stored function](#stored-functions) `name()`, whose value is the definition (a function of that name that is not a recommender is refused), so it is versioned, kept by checkpoints and copies, and shipped to replicas as functions are.
- **Fed from writes.** Every transaction that inserts rows into the events table, by SQL, `/v1/write`, an import or the Rust API, also adds to the counts at its commit, in the same transaction, so an event and its counts are written, logged and shipped together, and the next `recommend` sees them. An event placed between two older ones by its time takes back the pair they were and counts the two it makes. A row that replaces a row of its key (an upsert) is not a new event; deleting or updating events changes no count (`CREATE OR REPLACE RECOMMENDER` counts again). An event costs about `2 × window + 3` writes to the counts, each a read and a write by key, so writes of events take tens of times longer than without a recommender, and `CREATE` holds the writer while it counts the table (seconds per hundred thousand events). Counting at write rather than when asked keeps a call to a few reads by key whatever the size of the events table: counting a million events at each call would read them all.
- **`recommend(name, user [, k => 100, next => 1, co => 1, similar => 1, popular => 0.2])`** reads the user's row and builds four lists of up to 200 items: what came next after the user's last item (and, at 0.4, after the one before), the items seen together with each of the user's recent items (the more recent weighing more, each item's counts as shares of its own), the items nearest the user's last eight by `maxsim` over the items' vectors, and the most popular items. An item scores the sum over the lists of the list's weight over 60 plus its rank there (reciprocal rank fusion); a weight of 0 leaves its list out. The user's recent items are left out, and the best `k` (at most 10,000) are the ranking: the call is then [`ranked(...)`](#rankedkeys-a-ranking-from-elsewhere) of their keys, so a row it does not list scores `NULL`, and `ORDER BY recommend(...)` on its own answers only the rows it lists (in a sum or `rrf` with other scores, the others rank by those). A user with no events gets the popular items. A call reads about two rows by key and one ranked page per recent item, some milliseconds. The answer's plan lines begin with one for each call: `recommend shop for 'u42': 100 items, from 12 next, 85 seen together, 200 similar, 120 popular; 9 of the user's items left out`.
- **`FROM recommend(...)`** is a table of the items, best first, with `item`, `score`, and each list's rank of the item (`next`, `co`, `similar`, `popular`, `NULL` where it is not in a list); a `LIMIT` lowers `k`. It takes no join, `WHERE`, `ORDER BY` or `GROUP BY`: rank a table by `ORDER BY recommend(...)` for those.
- **Where it runs.** On one database, and on chloedb-server over its own tables (`CREATE` and `DROP RECOMMENDER` need an admin key). A coordinator refuses both the statement and the call, since a user's events are spread over shards ([cluster limits](cluster-limits.md)); counts of your own in an [edge table](#edge-tables) or a [next-items table](next-items.md) work there. A version of ChloeDB from before recommenders opens the database, but does not feed the counts.

## Chunks of a table's text: `chunks_of()`

> **Experimental.** This is new in 0.3.0, and its options and answers may change in a later release.

`chunks_of(table, text_field)` is a table of the pieces of each row's text, for a table of chunks that a vector field embeds, or a budgeted context packs from.

```sql
SELECT id, n, tokens, chunk FROM chunks_of(docs, body, max_tokens => 256, overlap => 32)
  WHERE lang = 'en' LIMIT 20;

CREATE TABLE chunks (id KEYWORD PRIMARY KEY, doc KEYWORD, n INT, body TEXT, tokens INT,
  emb VECTOR(512) OPTIONS (embed = 'body', model = 'potion-retrieval-32M'));
INSERT INTO chunks (id, doc, n, body, tokens)
  SELECT chunk_id, id, n, chunk, tokens FROM chunks_of(docs, body, max_tokens => 256);
```

- **Splits.** A text is cut where it says most: before a Markdown heading, then at blank lines, then after a sentence's end, then between words, and between characters only for a word longer than a chunk. The pieces are joined in order while a chunk holds at most `max_tokens` (256, from 16 to 8192), and a heading always starts a chunk. With `overlap => m` (0), each chunk after the first starts with about `m` tokens of the end of the one before, from a word's start.
- **Tokens** are estimated as a quarter of the characters, rounded up: close for English and most models' tokenizers, and the same on every machine.
- **Columns.** `id` (the row's key), `n` (0, 1, ...), `chunk_id` (`'<id>#<n>'`, a key for a table of chunks), `chunk`, `tokens` (with the overlap), and `start_char` and `end_char`, where the chunk's own text is in the row's, in characters. The query names some of them, or `*`.
- **The query** is read as written: its `WHERE` is a condition of the source table's rows, and its `LIMIT` cuts the chunks. It takes no join, `ORDER BY`, `GROUP BY` or `OFFSET`: write the chunks to a table to do those. `INSERT INTO t (columns) SELECT ... FROM chunks_of(...)` writes them, as `INSERT`s of 500 rows each. A coordinator reads the rows from its shards and splits them itself. The field is a stored `TEXT` field. A call reads as many of the table's rows as its `LIMIT` needs (each row with text gives a chunk at least; when rows without text leave the `LIMIT` unfilled it reads twice as many again), 1,000,000 at most without one, and what it reads may hold 1 GiB at most (or the query memory budget, if smaller): a read the database makes for itself is not held to a server's `--max-rows`, so this bounds it.

## Running SQL

The `chloedb` shell runs one statement, or reads statements from standard input. It opens a database that exists and does not create one; these lines read the demo catalogue that `chloedb-server --demo 20000 --db products.chloedb` makes:

```sh
cd chloedb && cargo build --release
target/release/chloedb products.chloedb "SELECT category, COUNT(*) FROM products WHERE MATCH(body, 'waterproof') GROUP BY category ORDER BY 2 DESC LIMIT 5" --plan
target/release/chloedb products.chloedb --param t='waterproof tent' < queries.sql
```

| flag | effect |
|---|---|
| `--param name=value` | binds `:name`: a number, a string, or a JSON array for a vector |
| `--plan` | prints the plan lines the executor recorded for each statement |
| `--repeat n` | runs each statement *n* times and reports the median time |
| `--read-only` | opens the database read-only, to read it while another process writes it ([readers in other processes](index.md#readers-in-other-processes)); without it, the shell opens a database another process has open to write read-only, and says so |

From Rust, `Db::query` takes a `SELECT` and a `Params` map, and `Db::sql` takes any statement, on any table, writes and `CREATE TABLE` included. Over the network, [chloedb-server](server.md) serves the same SQL over every table of a database.

## Tables

A database starts with the table it was created with, and `CREATE TABLE` adds more. An example, a table of orders:

```sql
CREATE TABLE orders (id KEYWORD PRIMARY KEY, customer KEYWORD, status KEYWORD, total FLOAT, note TEXT, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP);
```

`KEYWORD` holds exact values (ids, statuses), `TEXT` words searched with `MATCH`, `INT` and `FLOAT` numbers, `TIMESTAMP` times, and `VECTOR` embeddings; a type followed by `[]` holds a list. It needs an admin key. A larger example: Each table has its own columns, primary key, indexes and merges; the tables share the file, its write-ahead log and transactions.

```sql
CREATE TABLE authors (
  id KEYWORD PRIMARY KEY,
  name TEXT OPTIONS(analyzer = 'simple'),
  born INT,
  topics KEYWORD[],
  bio TEXT OPTIONS(positions = false),
  emb VECTOR OPTIONS(embed = 'name, bio', model = 'potion-retrieval-32M')
);
CREATE TABLE IF NOT EXISTS wrote (
  id INT PRIMARY KEY,
  author KEYWORD NOT NULL REFERENCES authors,
  paper KEYWORD REFERENCES papers (id),
  role KEYWORD NOT NULL DEFAULT 'author',
  added TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE events (
  id INT PRIMARY KEY,
  usr KEYWORD NOT NULL,
  item KEYWORD,
  ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP
) OPTIONS(shard_by = 'usr', cluster_by = 'usr', retain = '400 days');
DROP TABLE IF EXISTS wrote, authors;
```

- **Types** are `TEXT`, `KEYWORD`, `INT` (or `INTEGER`, `BIGINT`), `FLOAT` (or `DOUBLE`, `REAL`), `BOOL` (or `BOOLEAN`), `TIMESTAMP` (or `TIMESTAMPTZ`, `TIMESTAMP WITH TIME ZONE`, `DATETIME`, and `DATE`, which is a `TIMESTAMP`: a date written alone is its midnight UTC), `VECTOR(dimensions)` (1 to 65,536 dimensions), `SPARSE`, `MEDIA` and `EDGES`; `KEYWORD[]` and `INT[]` hold lists, and `VECTOR(dimensions)[]` several vectors per row ([multi-vector fields](#maxsimfield-vectors-several-query-vectors)). A `BOOL` or `TIMESTAMP` column holds one value and is neither the primary key nor a reference.
- **`OPTIONS(...)`** takes the options of the [JSON schema](server.md#schemas): `substring` (true: `LIKE` and `ILIKE` on the column read a [substring index](#substrings-like-and-ilike-anywhere-in-a-value)), `positions`, `stored` and `indexed` (true or false; an indexed `TEXT` or `SPARSE` field is stored unless generated, since merges rebuild its index from the stored values), `from` (the fields a generated `TEXT` field joins), `analyzer` (`'english'`, the default: stopwords dropped and words stemmed, so `running` finds `run`; `'simple'`: every word kept as written, for names and codes; `'standard'`: text in [any language and script](index.md#text-in-other-languages), Chinese, Japanese, Korean and Thai included; `'cjk'`: `standard` with pairs of characters as the terms of Chinese and Japanese; or a language, such as `'french'`, `'german'`, `'arabic'`, `'hindi'` or `'chinese'`, which is `standard` with what the language needs, a stemmer for most), `index` (`'hnsw'` with `m` and `ef_construction`, `'flat'`, or `'ivf'` with `nlist`), `partition_by` with `partition_bounds` (a graph per range or value of a column, for [filtered searches](#vector-partitions)), `codes` (`'sq8'`, `'binary'`, `'rabitq1'`, `'rabitq2'` or `'rabitq4'`; a multi-vector field searches `'binary'` by default), `keep_f32` (a multi-vector field keeps its float32 vectors to score with, four times the bytes of its 8-bit codes), `pool` (a multi-vector field merges a row's vectors whose cosine similarity is at least this, such as `0.9`, into their mean when it is written), `pool_factor` (a multi-vector field clusters a row's vectors into one group for every this many, such as `2`, by Ward's linkage, and keeps each group's mean when it is written; exclusive with `pool`), `kind` of a `MEDIA` field, `derive` (a `MEDIA` field) with `model` for [text made from media](server.md#text-from-media), `embed` with `model` for an [embedded vector field](server.md#embedded-vector-fields), which may leave out its dimensions when ChloeDB knows the model's (`VECTOR` alone), or an [embedded `SPARSE` field](server.md#term-weights-of-a-learned-sparse-model), and on a `KEYWORD[]` `semantic_id` (the vector field) with `codebook` for a [semantic-ID field](server.md#semantic-ids): `sid KEYWORD[] OPTIONS(semantic_id = 'emb', codebook = 'items-rq')`. Field lists are a string, `'name, bio'`, or `ARRAY['name', 'bio']`.
- **The primary key** is one column: `PRIMARY KEY` on it, or a `PRIMARY KEY (column)` constraint. A table without one only takes plain `INSERT`.
- **`REFERENCES table`**, or `FOREIGN KEY (column) REFERENCES table`, records that a `KEYWORD` or `INT` column holds primary keys of that table, which must exist (or be the table being created) and have a primary key of the column's type. References are not enforced: a row may name a key that is not there, and an inner join along the reference leaves that row out (`LEFT JOIN` keeps it). Joins along them are [ordinary joins](#subqueries-with-and-joins) (`FROM wrote w JOIN authors a ON w.author = a.id`).
- **`NOT NULL`** refuses a row without a value in that column, from any write: SQL, JSON, a file or the Rust API. **`DEFAULT value`** is the value a write that leaves the column out gets: a constant of the column's type on a single-valued `TEXT`, `KEYWORD`, `INT`, `FLOAT`, `BOOL` or `TIMESTAMP` column that is not the key, or `CURRENT_TIMESTAMP` (or `now()`) on a `TIMESTAMP`, the time of the statement, the same in all its rows. An `INSERT` that leaves a column out, a JSON row without its key and a CSV file without its column give it the default; a `NULL` that a write gives stays `NULL`, and a column with `NOT NULL` refuses it. In SQL the keyword `DEFAULT` stands for a column's default, in `VALUES` and in `SET`, and `CURRENT_TIMESTAMP` (or `now()`, `LOCALTIMESTAMP`) is the time of the statement there and in conditions (`SET seen = CURRENT_TIMESTAMP`, also in `ON CONFLICT ... DO UPDATE`). The Rust API writes a row as it is given; `Schema::defaults` returns a row of the defaults to start from. The JSON schema's options are `"not_null": true` and `"default"` (`"now"` for the time of the write).
- **`OPTIONS(shard_by = 'column')`** after the columns places the table's rows on [shards](server.md#tables-on-shards) by that column instead of by the primary key, so that a user's events sit on one shard together. The column is a stored, single-valued `KEYWORD` or `INT` with `NOT NULL`, other than the key, and a row keeps it: `UPDATE` does not change it (delete the row and insert it again). In JSON it is `"shard_by": "usr"`. One database places nothing, so the option only matters to a coordinator.
- **`OPTIONS(cluster_by = 'column')`** stores each segment's rows sorted by that column, rows of one value in the order they were written: a flush sorts the rows it writes, and a merge sorts the rows of the segments it joins. A user's history then sits in one run of each segment instead of scattered across it, so reading it touches a few pages rather than one per row. The column is an indexed, single-valued `KEYWORD`, `INT` or `TIMESTAMP`, and a row keeps it (`UPDATE` does not change it). Queries return the same rows either way; only `ORDER BY` fixes their order. In JSON it is `"cluster_by": "usr"`.
- **`OPTIONS(retain = '400 days')`** drops rows once their time is older than the period: a flush does not write them, and a merge leaves them out of the segment it writes, or drops a segment whose rows are all past it without rewriting or reading anything; the merger looks for such segments on its own. Such a table's segments merge only within windows of time, a thirtieth of the period each (from a second to a day): a segment holds rows of one window, as of its newest row, so a table kept 30 days holds segments of a day at most, and each leaves whole once its newest row is past the period rather than being rewritten as its rows expire. Until then a query still finds them, so filter on the time where that matters (`WHERE ts > '2025-10-01'`). The time is the table's only `TIMESTAMP` column, or the one `retain_by = 'column'` names (indexed, single-valued, and not changed by `UPDATE`); a row without one is kept. A period is a whole number and a unit: seconds, minutes, hours, days or weeks (`'90 s'`, `'12h'`, `'2 weeks'`). In JSON it is `"retain": "400 days"` with `"retain_by"`.
- **`OPTIONS(expire_by = 'column')`** expires each row at the time its column holds, as a key-value store's TTL does: from that moment on queries, counts, joins and reads by key leave the row out, and a write by its key finds no row there (an `INSERT` of the key succeeds, an `UPDATE` changes nothing). Rows without a time never expire, and `UPDATE ... SET until = ...` (or a `set` in place) moves a row's time. A server that takes writes deletes expired rows every few seconds, and `Db::expire` (Python: `expire()`) does it at once; those deletes are logged as any delete is, so [replicas](server.md#replication-a-primary-and-its-replicas) hold the same rows without reading a clock. Unlike `retain`, which drops old rows when segments are written and leaves them visible until then, an expired row is gone for reads at once. The column is an indexed, single-valued `TIMESTAMP`, the table has a primary key, and `DROP COLUMN` keeps the column (`RENAME COLUMN` renames the option too). It makes the database format 3, feature `expire_by`. In JSON it is `"expire_by": "until"`.

```sql
CREATE TABLE sessions (
  id KEYWORD PRIMARY KEY,
  usr KEYWORD,
  until TIMESTAMP
) OPTIONS(expire_by = 'until');
INSERT INTO sessions (id, usr, until) VALUES ('s1', 'u7', '2026-11-02T12:00:00Z');
```
- **`OPTIONS(compression = 'zstd')`** compresses the table's stored rows (its doc store) with zstd in 32 KiB blocks instead of LZ4 in 16 KiB ones. On four logs from Loghub the doc store took 32% to 39% less, and the database 9% to 16% less, while a read of one row by key took 12% to 40% longer (48 µs instead of 34 on 11M lines) and a scan of every row about as long (`chloedb/results/log-bytes/`, on a 4-core cloud container). It suits tables that are written far more than read row by row, such as logs; queries that count, filter, group or search without returning rows read no stored rows either way. Segments written after the option use it, and older ones keep LZ4 until a merge rewrites them. It makes the database format 3, feature `zstd`. In JSON it is `"compression": "zstd"`; `'lz4'` is the default.
- **`OPTIONS(edges = 'src, dst, w')`** makes each row an edge of a graph over another table's rows, which walks over that table follow ([edge tables](#edge-tables)). In JSON it is `"edges": "src, dst, w"`.
- **`DROP TABLE`** drops tables other than the first. A table that another table references goes with it (`DROP TABLE wrote, authors`) or with `CASCADE`, which removes those references.
- Every other clause (`UNIQUE`, `CHECK`, `ON DELETE`, constraint names, a `DEFAULT` that is an expression, `CREATE TABLE ... AS SELECT`) is an error.

`CREATE TABLE`, `ALTER TABLE` and `DROP TABLE` run through `Db::sql`, Python's in-process [`chloedb.Database`](python.md), and [chloedb-server](server.md#schemas)'s `POST /v1/sql`, where a coordinator runs them on every shard.

### Changing a table's columns

```sql
ALTER TABLE products ADD COLUMN stock INT NOT NULL DEFAULT 0;
ALTER TABLE products ADD COLUMN IF NOT EXISTS brand KEYWORD, ADD note TEXT;
ALTER TABLE products ADD COLUMN emb2 VECTOR OPTIONS(embed = 'title', model = 'potion-retrieval-32M');
ALTER TABLE products RENAME COLUMN note TO notes;
ALTER TABLE products DROP COLUMN IF EXISTS notes;
ALTER TABLE IF EXISTS drafts DROP COLUMN body, DROP COLUMN title;
```

`ALTER TABLE` changes the columns of any table, the first included, while it holds rows; the changes of one statement apply in order, as one change.

- **`ADD COLUMN`** (or `ADD`) takes a column as `CREATE TABLE` does, with its type, `NOT NULL`, `DEFAULT`, `REFERENCES` and `OPTIONS`, and puts it last. Rows written before it read it as its default, or `NULL` without one (a `TIMESTAMP` with `DEFAULT CURRENT_TIMESTAMP` too: the default is the time of each later write): in `SELECT`, `WHERE`, `ORDER BY`, aggregates and searches alike. Writes from then on take it. A `NOT NULL` column without a default can only be added to a table without rows. A column added with `embed` gets its model and embeds the rows written from then on; the rows before get theirs from [the backfill](server.md#embedded-vector-fields), which a server runs once the model opens (from Rust, `Db::embed_backfill`), and until then have none. A generated `TEXT` column, a semantic-ID column, `EDGES` and a primary key cannot be added.
- **`DROP COLUMN`** drops a column and its values; a column added again under the same name is a new column, `NULL` in the older rows. The primary key, a table's only column, a column another field names (as the source of a generated text, an embedding, a caption or semantic IDs) and one the table's options name (`shard_by`, `cluster_by`, `retain_by`, `expire_by`, `edges`) stay, and the statement says what names it; so do columns a [rank profile](#rank-profiles-a-ranking-kept-with-its-table) of the table ranks by.
- **`RENAME COLUMN a TO b`** keeps the column's values, and the fields and options that name it follow it; a rank profile does not, so a column a profile ranks by keeps its name until the profile is dropped or replaced.
- **`IF EXISTS`** after `ALTER TABLE` skips a table that is not there; `ADD COLUMN IF NOT EXISTS` and `DROP COLUMN IF EXISTS` skip a column that is, or is not. Positions (`FIRST`, `AFTER`), `ALTER COLUMN`, `RENAME TO` (of the table) and `CASCADE` are errors.

A change writes nothing but the table's schema: it waits for writes in flight, writes the memtable as segments (a write-ahead log names columns by position), and commits the new schema. A segment written before a column was added does not hold it; when it is opened, the column's index is built in memory over its rows' default, and the next merge that rewrites the segment writes the column for real. A dropped column's values stay in the older segments, unread, until merges rewrite them. The Rust API is `Db::alter_table(table, &[Alter::AddField(field), Alter::DropField(name), Alter::RenameField { from, to }])`; a database with a column added later needs the feature `added_fields`, so a version of ChloeDB from before it refuses the database.

On chloedb-server, a coordinator runs `ADD COLUMN` and `RENAME COLUMN` on every shard and then serves the table as altered; it refuses `DROP COLUMN`, since its plans name a table's columns by position and a query planned while some shards had dropped the column would read the next one. A primary ships the change to its replicas in its place among the writes, as it ships `CREATE TABLE` and `DROP TABLE`, and its replicas give the table the same columns under the same field ids ([replication](server.md#replication-a-primary-and-its-replicas)).

## Writes

```sql
INSERT INTO products (id, title, price, tags, emb) VALUES ('p1', 'red mug', 12.5, ARRAY['kitchen', 'gift'], :v);
INSERT OR REPLACE INTO products (id, title, price) VALUES ('p1', 'red mug, large', 14.0);
INSERT INTO products (id, title, price) VALUES ('p2', 'blue mug', 11.0) ON CONFLICT (id) DO NOTHING;
INSERT INTO products (id, title, price) VALUES ('p1', 'red mug', 12.0)
  ON CONFLICT (id) DO UPDATE SET price = excluded.price, stock = stock + 10;
UPDATE products SET stock = stock - 1, price = 11.0 WHERE id = 'p1';
UPDATE products SET stock = DEFAULT WHERE id = 'p2';
UPDATE products SET price = round(price * 0.9, 2), title = title || ' (sale)' WHERE tags = 'gift';
INSERT INTO products (id, stock) VALUES ('p1', 5)
  ON CONFLICT (id) DO UPDATE SET stock = coalesce(stock, 0) + excluded.stock;
DELETE FROM products WHERE MATCH(title, 'discontinued');
DELETE FROM products WHERE stock = 0 ORDER BY price LIMIT 100;
```

- A write names its table, any table of the database.
- **`INSERT`** fails on a primary key that already exists. **`INSERT OR REPLACE`** (or `REPLACE INTO`) replaces that row, an upsert. **`ON CONFLICT (key) DO NOTHING`** (or `INSERT OR IGNORE`) writes only the rows whose key is new, and the first of a key the statement repeats. **`ON CONFLICT (key) DO UPDATE SET ...`** inserts the new keys and sets fields of the rows already there, where `excluded.column` is the value the statement gave; a key that comes twice in one such statement is an error. On a cluster, a table placed by `shard_by` keeps its keys unique across shards for `INSERT`, `INSERT OR REPLACE` replaces a row only on its `shard_by` value's shard, `ON CONFLICT DO UPDATE` takes rows of one shard, and `UPDATE`/`DELETE ... LIMIT` pick their rows first ([cluster limits](cluster-limits.md)). Lists are `ARRAY[…]`; a vector is a parameter, `ARRAY[…]` or a JSON array string; a multi-vector value is a list of vectors (`ARRAY[ARRAY[…], ARRAY[…]]`, a parameter holding a list of vectors, or `'[[…], […]]'`), and one vector given to it is a list of one; a `SPARSE` value is a JSON object string, and a `MEDIA` value a URI or a JSON object string.
- **`UPDATE`** takes any `WHERE`, and a `LIMIT`. Setting only single-valued `INT` and `FLOAT` fields (`SET clicks = clicks + 1`) changes them in place, without rewriting the row, which suits counters and prices that change often. Setting any other field rewrites the row, which needs every text field stored or generated.
- **`SET`** takes a value or any expression of the row: arithmetic, `||`, `CASE`, `NOT`, comparisons, `lower`, `upper`, `length`, `abs`, `round` and `coalesce` over the row's columns, and in `ON CONFLICT DO UPDATE` over `excluded.column` too. Every expression reads the row as it was, so `SET a = b, b = a` swaps two fields. The result is stored as the field's type: a whole number fits an `INT` (`clicks = price * 2`), but `7 / 2` is 3.5 and an error there (`round(clicks / 2)` fits); one value set to a multi-valued field becomes a list of it. NULL in gives NULL out, as in SQL, except `field + number` and `field - number`, which count a NULL as zero, as a counter does; `coalesce(field, 0) + other` does the same elsewhere. A multi-valued field without values is NULL to `coalesce` and `IS NULL`. Searches, scores and aggregates (`MATCH`, `bm25`, `score()`, `COUNT`) are errors in `SET`. A statement that cannot set one of its rows sets none.
- **`DELETE`** takes any `WHERE`, and `ORDER BY` with `LIMIT` to delete the first rows in that order.
- `RETURNING`, `UPDATE ... FROM`, `DELETE ... USING`, `INSERT ... SELECT`, `ON DUPLICATE KEY UPDATE` and `DO UPDATE ... WHERE` are errors.

### Loading a file: CSV, TSV, JSON lines

Many rows at once come from a file rather than `INSERT`s: `POST /v1/import?table=orders` with the file as the body and its content type (`text/csv`, `text/tab-separated-values`, `application/x-ndjson` or `application/json`), or the MCP tool `import` with the file's text in `data` (or the rows as objects in `rows`). A CSV's first line names the columns; columns the table does not have are skipped and named in the answer, and an empty cell is the column's `DEFAULT` (`?empty=null` makes it `NULL`). The table must exist first (`CREATE TABLE`, above), or, with an admin key, the MCP tool's `create: true` makes it from the rows: whole numbers become `INT`, numbers `FLOAT`, `true`/`false` `BOOL`, ISO dates `TIMESTAMP`, text of two and a half words or more on average `TEXT` and other text `KEYWORD`, with a column named `id` (or the one `key` names) as the primary key, and the answer shows the `CREATE TABLE` it ran. Parquet and SQLite files load the same way ([server](server.md#loading-rows)), and the command center's Import page takes a file dropped on it.

```bash
curl -X POST "$URL/v1/import?table=orders" -H "authorization: Bearer $KEY" -H "content-type: text/csv" --data-binary @orders.csv
```

Each statement is one transaction, and returns the number of rows it wrote. From Rust, the transaction API (`Database::begin`, `WriteTxn::insert`, `upsert`, `set`, `delete`, `commit`, and `WriteTxn::table` for the other tables) groups several writes into one, across tables too; `WriteTxn::upsert_if_newer` keeps whichever version of a row has the highest version number, for replaying a feed out of order.

## EXPLAIN

`EXPLAIN <statement>` shows the plan of a `SELECT`, `INSERT`, `UPDATE` or `DELETE` without running it: a row per line, in a column named `plan`, the root first and each operator's input indented under it. It runs in-process (`db.sql` in Python, `Db::sql` in Rust); over HTTP, send the statement without `EXPLAIN` to [`POST /v1/explain`](server.md#http-api), which answers the same lines as `tree`.

```sql
EXPLAIN SELECT category, COUNT(*) AS n FROM papers WHERE published >= '2026-10-01'
GROUP BY category HAVING COUNT(*) >= 2 ORDER BY n DESC, category LIMIT 5;
```

```text
Project: category, COUNT(*) AS n
  Limit: 5
    Sort: COUNT(*) DESC, category
      Filter: COUNT(*) >= 2.0
        Aggregate by category: COUNT(*)
          Search papers
            filter: published >= TIMESTAMP '2026-10-01T00:00:00Z'
```

A `Search` is the rows of one table that its `filter` holds for, ranked by its `order`; the operators above it group, keep, sort, cut and name them, and a write's operator (`Insert`, `Update`, `Delete`) sits on top. The plan is the statement as bound: values have their field's type, and a `NOT` over a field that can be null also says `x IS NOT NULL`, since a row without a value matches neither a comparison nor its negation.

`EXPLAIN FORMAT JSON` (or `EXPLAIN (FORMAT JSON)`) gives the plan as one row of JSON: fields by their position in the table, typed values (`{"int": 5}`, `{"text": "5"}`), a version and the features the plan needs. ChloeDB refuses a plan from a newer version by name. `EXPLAIN ANALYZE` and other formats are refused.

A compound query's plan has operators over rows in memory above its `Search`es, and its JSON needs the feature `operators`:

```sql
EXPLAIN SELECT a.id, b.title FROM papers a JOIN papers b ON a.cites = b.id WHERE a.year >= 2019 ORDER BY a.year DESC LIMIT 5;
```
```text
Project: id AS "a.id", title AS "b.title"
  Limit: 5
    Join on targets(cites) = id
      Project: id, year, cites
        Search papers
          filter: year >= 2019
          order: year DESC
      Project: title, id
        Search papers
```

`Join` (or `Left join`) matches its two inputs on the keys after `on`, `targets(f)` being the rows an `EDGES` field links to, with any other `ON` condition after `AND`; `With` names its results, which `Ref` reads; `Key order of top` puts rows in the order of the `WITH` result `top`; `Distinct` keeps each row once, and `Distinct on: video` the [first row of each group](#the-first-row-of-each-group-distinct-on). A subquery sits in its condition: `IN values of (...)` (its values), `IN keys of (...)` (its rows' keys), `EXISTS (...)` and a scalar `(...)`, each shown on one line with `;` between its operators. `SELECT DISTINCT` of the table alone is a `GROUP BY`, a single-table query.

Every statement runs as its plan, and a plan in JSON runs on its own: `Db::run` in Rust, `Database.run` in [Python](python.md#querying) and [`POST /v1/plan`](server.md#http-api) over HTTP take one, and check it against the table's schema as the SQL binder checks a statement.

## Reading a plan

`--plan` shows what the planner decided, segment by segment. The same statement can get different plans as the data changes. In the evaluation, an `OR` filter matching about 15% of documents was scanned exactly on SciFact's 5,183 documents, but on the larger collections the planner measured 11% near the query and post-filtered the HNSW result. An RRF query's topic filter matched 11% of Quora globally but only 3.4% near the query, so the planner switched to an exact scan of its 58,767 members, at 3.1 ms. [Planning and execution](index.md#planning-and-execution) describes the choices.
