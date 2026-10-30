---
title: Query language
parent: Strata
nav_order: 1
---

# Strata's query language
{: .no_toc }

Strata speaks standard SQL over the tables of a database: `SELECT` with functions for full text, vectors, rank fusion and graphs, aggregates, typo-tolerant and synonym searches, and subqueries, `WITH` and joins of a table with itself; `INSERT`, `UPDATE` and `DELETE`; and `CREATE TABLE` and `DROP TABLE`. Text, structured and vector conditions mix freely in one statement, and the planner decides how to evaluate them together.
{: .fs-5 .fw-300 }

1. TOC
{:toc}

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

-- hybrid: reciprocal rank fusion, or a weighted sum of normalised scores
SELECT id FROM papers WHERE year >= 2015
ORDER BY rrf(bm25(body, :text), similarity(emb, :q)) DESC LIMIT 10;

SELECT id FROM papers WHERE year >= 2015
ORDER BY 0.2 * minmax(bm25(body, :text)) + 0.8 * minmax(similarity(emb, :q)) DESC LIMIT 10;

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
SELECT [DISTINCT] <columns | * | score() | hops() | seq_no() | features() | ranking expressions | aggregates>
FROM <table> [JOIN <table> ON ...] | <WITH name> | (SELECT ...) AS t
[WHERE <predicate>]
[GROUP BY <columns> [HAVING <predicate over aggregates>]]
[ORDER BY <score, column or aggregate> [ASC | DESC] [NULLS FIRST | LAST] [, <tie-break> [ASC | DESC]]]
[LIMIT n [OFFSET m]]
```

- **Tables by name.** A database holds one or more [tables](#tables), and a query reads the tables it names (in any case): one, or several joined, read by subqueries or named in `WITH` ([below](#subqueries-with-and-joins)).
- **Quoted names.** A table or column name in double quotes is the name inside them, so any name can be written: `"order"` (a word SQL reserves), `"doc no"`, `"say ""hi"""` (a quote doubled), and `FROM "papers"` reads papers. Python's [`table()`](python.md#queries-built-in-python) builds queries with no quoting at all.
- **Strings** are single-quoted, `'sale'`. Where a value goes, a double-quoted `"sale"` is a string too, as SQLite and MySQL read it, and a bare name is an error rather than a string: `WHERE tag = sale` says `no such column: sale`.
- **Clauses Strata does not run are errors**, never ignored: `FETCH`, `FOR UPDATE`, `TOP`, `SELECT INTO`, `WINDOW`, `QUALIFY`, `GROUP BY ... WITH ROLLUP` and the like.
- **`SELECT *`** returns every stored, non-vector column.
- **`score()`** is the value of the `ORDER BY` score for each row; **`hops()`** is a row's distance in a graph traversal ([below](#graphs)); **`seq_no()`** is the sequence number of the write that last changed the row, in a database that numbers its writes, for [conditional writes](server.md#conditional-writes). It is a column of a query on one table, and cannot be aggregated or ordered by. A [ranking expression](#features-the-values-a-ranking-read) that scores each row by itself is a column too (`bm25(title, :q) AS t`), and **`features()`** stands for the inputs of the query's ranking.
- **`ORDER BY`** takes one key, plus an optional tie-break that must be the primary key (or, in a facet query, the group column). Ranking by a score is always best first; `ORDER BY distance(…) ASC` ranks like `similarity(…) DESC`.
- **Parameters** are `:name` placeholders, bound from the API or from the shell's `--param name=value`. A vector parameter is a JSON array.
- **Limits.** An expression nests at most 1,000 levels, counting parentheses and the operators of a chain such as `a OR b OR …` (SQLite has the same default); a long list of values belongs in `IN (…)`, which has no such limit. A `LIMIT` larger than the table returns every row that qualifies. A query ranked by a score without a `LIMIT` returns 100 rows, counted after its `OFFSET`, on one database and through a coordinator alike.

## Columns and types

| type | indexed as | filters |
|---|---|---|
| `INT`, `FLOAT` (finite: no NaN or infinity) | frame-of-reference column with zone maps, range histogram | `=`, `!=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IS [NOT] NULL` |
| `KEYWORD` | dictionary codes and one docID list per value | `=`, `!=`, `IN`, `NOT IN`, `IS [NOT] NULL`; `GROUP BY` |
| `BOOL` | an `INT` column of 1 and 0 | the field alone (`WHERE done`, `NOT done`), `=`, `!=`, `IS [NOT] TRUE`, `IS [NOT] FALSE`, `IS [NOT] NULL`; `GROUP BY` |
| `TIMESTAMP` | an `INT` column of microseconds since 1970-01-01 UTC | as `INT`, with times written as text: `at >= '2026-10-01'` |
| `TEXT` | inverted index (optionally with positions); may be generated from other text fields | `MATCH` |
| `VECTOR(d)` | float32 rows plus 8-bit codes (or 1-bit codes, `"codes": "binary"`), with HNSW, IVF or flat search; may be declared as a model's embedding of other fields ([embedded vector fields](server.md#embedded-vector-fields)) | ordering by `similarity` / `distance`; a similarity graph |
| `SPARSE` | inverted index of learned term weights (SPLADE and similar) | ordering by `similarity` |
| `MEDIA` | a file by reference: URI, SHA-256, type, size; a `KEYWORD` index of the hashes | `=`, `IN` on the hash; `GROUP BY` |
| `EDGES` | out-edge CSR and reverse index per segment | `REACHABLE`, `PATH`, `ppr` |

Predicates combine with `AND`, `OR`, `NOT` and parentheses, across all of these.

`INT` values are 64-bit integers and compare exactly: `id = 9007199254740993` finds that row and not 9007199254740992, which a double cannot tell apart. A fraction bounds integers by rounding inward (`n > 2.5` is `n >= 3`, `n = 2.5` matches nothing).

`NULL` follows SQL's three-valued logic. A comparison with a missing value is neither true nor false, and so is its negation: `rating != 2` and `NOT rating > 2` leave out the rows without a rating. Ask for them with `rating IS NULL` (`IS NOT NULL` for the others). `MATCH` and graph predicates are always true or false, so `NOT MATCH(body, 'x')` includes rows without a body. A filter on a multi-valued field holds when some value matches, and its negation when none does, so `tags != 'red'` includes rows without tags (`tags IS NULL` finds those). `rating = NULL` is an error that points to `IS NULL`.

A `BOOL` is `TRUE` or `FALSE`; a write also takes 1 and 0 and the text `'true'`, `'false'`, `'t'`, `'f'`, `'1'` and `'0'`, and refuses anything else. A field named alone is a condition (`WHERE done AND NOT archived`), with `NULL` neither true nor false: `NOT done` and `done != TRUE` leave out the rows without a value, `done IS NOT TRUE` takes them in. `SUM(done)` counts the trues, and answers come back as booleans.

A `TIMESTAMP` is a moment, kept to the microsecond in UTC and written as ISO 8601 text: a date (`'2026-10-01'`, midnight UTC) or a date and time (`'2026-10-01T12:30:00Z'`, `'2026-10-01 14:30:05.25+02:00'`), with an offset or none (UTC). A number is refused, so seconds are not mistaken for microseconds. Comparisons, `BETWEEN`, `IN`, `ORDER BY`, `MIN`, `MAX` and `COUNT` work on it (`SUM` and `AVG` do not), and answers are ISO 8601 text in UTC, such as `2026-10-01T12:30:05.250Z`. In `decay(at, origin, scale)` on a `TIMESTAMP`, the origin is a time or `now()`, and the scale and offset are seconds: `decay(published, now(), 2592000)` halves a row's weight for each 30 days of age. A database whose tables use `BOOL` or `TIMESTAMP` is written in [file format 3](index.md#wal-memtable-and-snapshots), which versions of Strata before these types refuse.

`KEYWORD` and `INT` fields may be multi-valued (`"multi": true` in the schema): a row holds a list, such as `ARRAY['red', 'blue']`, and a filter matches a row if any of its values matches. A facet counts each distinct value once per row.

A `KEYWORD[]` field may hold the [semantic ID](server.md#semantic-ids) of a vector field: every prefix of a code such as `12.7.201.0`, filled in by the writer, so `sid = '12.7'` finds the rows under a prefix. `sid_at(sid, n)` is the first `n` numbers of a code (`sid_at(sid, 2)` is `'12.7'`), in `SELECT`, `WHERE` and `GROUP BY`.

A `SPARSE` value is a JSON object of terms and non-negative weights, `{"tok": 1.25, "##en": 0.4}`, as a learned sparse model (SPLADE, BGE-M3's sparse output, OpenSearch's neural sparse models) writes it. Terms are taken as given, not analysed, and weights are stored in steps of 1/1024.

A `MEDIA` value names a file, `{"uri": "https://cdn.example.com/a.png", "sha256": "…", "mime": "image/png", "bytes": 48213, "width": 640, "height": 480}`, and a write may give only its URI, `'https://cdn.example.com/a.png'`, or its bytes, `'{"data": "<base64>"}'`, for the server to fill in ([media fields](server.md#media-fields)). Its index holds the hash: `image = '9f86d0…'` finds the rows of that file. `OPTIONS(kind = 'image')` takes only images (`'audio'`, `'video'`, `'pdf'`), whose vectors a `VECTOR` field can hold: `emb VECTOR OPTIONS(embed = 'image', model = 'siglip-base-patch16-224')` ([vectors of images](server.md#vectors-of-images)).

## Full text: `MATCH`

`MATCH(field, query)` is a boolean predicate. The query grammar follows SQLite FTS5:

```text
query   := and ("OR" and)*
and     := unary (["AND"] unary)*          juxtaposition is AND
unary   := ("NOT" | "-") unary | primary    "a NOT b" is "a AND NOT b"
primary := "(" query ")" | "\"phrase\"" | word | word* | word~ | word~N
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

Operators are uppercase; lowercase `and`, `or` and `not` are ordinary words (and stopwords). Parentheses and `NOT`s nest at most 100 deep. Words and phrases are analysed like the field's documents, by its [analyzer](#tables): a word that analyses to several terms (`e-mail`) becomes a phrase, and one that analyses to none (a stopword) is dropped. A prefix is lowercased and folded but not stemmed, and matches the stemmed terms that start with it.

## Scores

### `bm25(field, text)`

BM25 of the analysed terms of `text`, disjunctively: a row scores if it contains any of them. Ranked queries drop English function words (NLTK's 153-word list), except that its single letters (`d`, `m`, `o`, `s`, `t`, `y`) are kept unless they end a contraction: "vitamin d" and "T cell" keep their letter, "don't" and "it's" drop it. A term written twice weighs twice. Used in `ORDER BY` without a `MATCH`, it is a ranked search; with one, it ranks the matching rows.

| option | default | effect |
|---|---|---|
| `k1`, `b` | 1.2, 0.75 | BM25 parameters (`k1 >= 0`, `0 <= b <= 1`) |
| `algo` | planner's choice (MaxScore) | `'maxscore'`, `'bmw'` (Block-Max WAND) or `'exhaustive'` |
| `stopwords` | 1 (0 on a `simple` field) | `0` keeps English function words in the query, `1` drops them |
| `fuzzy` | 0 | `1`, `2` or `'auto'`: also score the indexed terms that many edits from each word ([typos](#typos-and-synonyms)) |
| `synonyms` | 0 | `1`: also score each word's synonyms |

Keeping function words is better for short questions: on Quora it gains 0.055 nDCG@10, while on TREC-COVID, whose queries are full questions, dropping them gains the same amount.

### `similarity(field, vector)` and `distance(field, vector)`

The dot product (cosine for unit vectors) and the distance between a row's vector and the query vector, which is a bound parameter or a JSON array of the field's dimension.

| option | effect |
|---|---|
| `strategy` | `'auto'` (default: the planner decides), `'prefilter'` (evaluate the filter, scan its members exactly), `'index'` (search HNSW or IVF with the filter inside), `'postfilter'` (search with a larger *k* and drop non-matching rows), `'flat'` (exact scan) |
| `ef` | HNSW search breadth (default 64) |
| `nprobe` | IVF clusters probed |
| `rerank` | how many candidates are re-scored in float32 |

A `VECTOR` field keeps, besides its float32 rows, a compact code per row that the search scans: 8 bits per dimension by default, or 1 bit per dimension with `"codes": "binary"` in the field's schema (or the build option `vector.codes = 'binary'`). Binary codes are 32 times smaller than the vectors and 8 times smaller than 8-bit codes; to make up for the coarser scores, the search re-scores 8 times as many candidates in float32. On synthetic 768-dimensional data that kept recall@10 at 0.985–0.994 with flat, IVF and HNSW search, at an eighth of the memory ([results](results.md#compressed-vectors)); check recall on your own embeddings before choosing them, since some models lose more to one-bit codes than others.

### `similarity(field, weights)` on a `SPARSE` field

The dot product of a row's term weights with the query's, `Σ q_t · w_t` over the terms both have. The query is a JSON object of term weights, bound as a parameter or written as a string:

```sql
SELECT id, score() FROM docs
ORDER BY similarity(splade, '{"vaccine": 1.4, "mrna": 2.1, "efficacy": 0.6}') DESC LIMIT 10;
```

It runs on the same machinery as `bm25` (MaxScore, Block-Max WAND, the filter strategies), so `algo => 'maxscore' | 'bmw' | 'exhaustive'` applies, and it fuses with other scores in `rrf`, weighted sums and products. `distance` is not defined on a `SPARSE` field.

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

`dot(a, b)` is the dot product of two vectors as a value, for rows in memory: `SELECT id, dot(emb, :q) FROM h` over a `WITH` result, a join, or a table's own rows. It equals `similarity` to the bit.

### `rrf(s1, s2, …)`: reciprocal rank fusion

Each input ranks the rows that pass `WHERE`, to a depth of 100 by default, and a row's fused score is

$$
\mathrm{rrf}(d) = \sum_{i} \frac{1}{k + \mathrm{rank}_i(d)}, \qquad k = 60
$$

Options: `k => 60`, `depth => 100`. The depth is raised to `LIMIT + OFFSET` when the page asks for more rows, as in weighted sums and products.

### Weighted sums: `minmax()`

A weighted sum of min-max normalised scores is weighted fusion: `0.2 * minmax(bm25(body, :t)) + 0.8 * minmax(similarity(emb, :q))`. Weights are numeric constants; `+` and `-` combine any number of terms. Candidates are the union of each input's top 100, re-scored exactly by every input and min-max normalised over that union:

$$
\mathrm{fused}(d) = \sum_i w_i\,\frac{s_i(d) - \min_{C} s_i}{\max_{C} s_i - \min_{C} s_i}
$$

Options, given on any input: `depth => 100` (candidates per input), `exact => 1` (score every row that passes `WHERE` instead of the top-100 union). The top-100 shortcut recovers 89–95% of the exact fused top 10 ([results](results.md#hybrid-ranking)).

A row without a value of a column input (`0.7 * minmax(bm25(body, :t)) + 0.3 * minmax(rating)` for a row whose `rating` is `NULL`) takes that input's worst value among the candidates: its lowest when the weight is positive and its highest when the weight is negative (0 and 1 after min-max), so a missing value never lifts a row.

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
| `bm25`, `similarity`, `ppr`, `decay`, `rrf`, a numeric column, `hops()`, `seq_no()` | the leaves: a row's value of each |
| `ln(x)`, `log(x)` (base 10), `log(b, x)`, `exp(x)`, `sqrt(x)`, `pow(x, y)`, `abs(x)`, `round(x[, digits])`, `sigmoid(x)`, `clamp(x, lo, hi)` | numeric functions |
| `least(…)`, `greatest(…)`, `coalesce(…)` | over their arguments that are not `NULL` |
| `minmax(x)` | `x` over the rows ranked, put on [0, 1]: 1 where all are equal, 0 where `x` is `NULL` |
| `rank(x)` | 1 plus the number of rows ranked with a greater `x`; `NULL` places after every other |
| `CASE WHEN c THEN x … ELSE y END` | the first branch whose condition holds (any `WHERE` condition), else `y`, else `NULL` |

`NULL` is a missing value (a `NULL` column, a row without a vector, one a traversal does not reach) or a result that is not a finite number (`ln(0)`, a division by 0, an overflow). Arithmetic and functions of `NULL` are `NULL`, except `coalesce`, `least` and `greatest`, which skip it. A row whose score is `NULL` ranks after every other, and its `score()` is `NULL`.

Candidates are the top `depth` (100) rows of each relevance leaf (`bm25`, `similarity`, a graph's), and of every input of `rrf`; an expression with no such leaf (`rating * 2 + year / 1000`), or `exact => 1` on any leaf, ranks every row that passes `WHERE`. A [rank-safe boost](#rank-safe-boosts) ranks every row its text score matches. Each leaf scores every candidate exactly, and `minmax()` and `rank()` are taken over the candidates. Over shards, each shard returns its candidates' leaf values and the coordinator runs the expression, so a split collection ranks like the whole one.

#### Rank-safe boosts

An expression with one text score (`bm25`, or `similarity` on a `SPARSE` field) whose other parts are numeric columns, `decay`, `CASE` conditions, numbers and `seq_no()` finds its exact top rows: the rows that scoring every row the text matches would rank first, not only the best of the text's top 100. `minmax()`, `rank()`, `rrf` and `exact => 1` take the candidates above instead.

```sql
-- the 10 best by BM25 boosted by importance, even a row 5,000th by BM25 alone
SELECT id FROM docs ORDER BY bm25(body, :t) * (1 + 0.2 * importance) DESC LIMIT 10;
```

The text score's top-k loop (MaxScore, Block-Max WAND or exhaustive, as `algo` says) ranks by the whole expression. From the k-th best value so far and each column's smallest and largest value, it works out the lowest text score a row needs, and skips the rows below it. A row the text does not match is not a candidate. When the expression does not rise with the text score (`rating - bm25(body, :t)`), no text score can be ruled out: the loop scores every matching row, and the plan line says `every match scored`. Over shards, each shard returns its own exact top rows, so a split collection ranks like the whole one.

A plan that ranks by an expression needs the plan feature `rank_expressions`; a version of Strata from before it refuses the plan by that name. Weighted sums of `minmax()`, products of scores and `rrf` are written as before, so older shards still run them.

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

The planner makes one search of them, with phases. Each phase ranks the rows of the one before by an expression, best first, and may read any of the table's columns, since the rows are the table's. Every row a phase reads goes on: one the expression gives no value ranks last with a `NULL` score (and a row a `bm25` phase's terms miss scores 0), and rows tied keep the order the phase before gave them. `LIMIT` and `OFFSET` on the query cut the last phase, and `score()` is the last phase's score. The first `WITH` may filter (`WHERE`) and rank as any query does; each later step reads the step before alone, with no `WHERE`, grouping or subquery, ranks by one expression `DESC`, and each `WITH` keeps the table's columns (`*`) and has a `LIMIT`. Other `WITH` queries plan as before: the table restricted to the rows of the `WITH`.

A phase whose expression is row-local is a second phase: over shards, each shard scores it on its own top rows by the first phase. A phase with `minmax()`, `rank()`, `rrf` or a weighted sum reads every row it reranks, so it is a global phase: it runs once over the merged rows (on the coordinator over shards), and so does every phase after it. The coordinator keeps the top rows of the whole collection by the first phase before it ranks them by the next, so a split collection answers exactly as the whole one does. `EXPLAIN` shows each phase and which kind it is, and a query's plan lines say how many rows each reranked. The first phase's `LIMIT` counts against the rows a query may reach (`max_rows`), and the phases against its deadline.

From Python, `rerank(expr, count)` adds a phase: `t.select("id", score()).order_by(bm25("body", q)).rerank(col("rating"), 100).limit(10)`. A plan with phases needs the plan feature `rank_phases`.

#### Features: the values a ranking read

A ranking expression that scores each row by itself is a column, with the value the ranking computes for the row, and `features()` lists the inputs of the query's ranking, to log beside what users chose and train a model on (Vespa's match-features):

```sql
SELECT id, score(), features() FROM docs
ORDER BY bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0)) DESC LIMIT 10;
-- columns: id, score(), bm25(body, :t), rating, bm25(body, :t) * (1 + 0.2 * coalesce(rating, 0))

SELECT id, bm25(title, :t) AS title_bm25, ln(1 + rating) AS r, year / 1000 FROM docs
ORDER BY similarity(emb, :q) DESC LIMIT 10;
```

- **`features()`** stands for one column per input, each once, named by its SQL text: every leaf of the ranking (a score such as `bm25(body, :t)` or `similarity(emb, :q)`, a column, `decay(..)`, `hops()`), the first phase's first, then each phase's expression that scores a row by itself and is not a leaf. With [rerank phases](#rerank-phases) it lists every phase's. An expression with `minmax()`, `rank()`, `rrf` or a weighted sum ranks rows against each other, so it is not a column: its inputs are, and `score()` is its value. A ranking by `distance(..)` lists the `similarity(..)` it ranks by. `features()` takes no alias and needs an `ORDER BY`.
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

- **The ranking** is a query of the table that ranks every row: `SELECT * FROM table ORDER BY expression` (any [ranking expression](#ranking-expressions), or a column with `ASC` or `DESC`), or [rerank phases](#rerank-phases) over it. It has no `WHERE`, and no `LIMIT` after its last ranking: the query that ranks by it filters and cuts. `WITH (name => value, ...)` after it gives its parameters defaults: numbers, strings, `TRUE` and `FALSE`. Names are letters, digits and `_`, in any case; `CREATE` refuses a name the table has already, and `CREATE OR REPLACE` replaces it.
- **`ORDER BY rank_profile('name', parameter => value, ...)`** is the whole `ORDER BY` of a query of the profile's table, without `ASC`, a `WITH`, a join or `GROUP BY`. The query ranks as if it were written out: its `WHERE` filters the rows the first ranking reads, and its columns, `LIMIT` and `OFFSET` are the last ranking's, so `score()` and [`features()`](#features-the-values-a-ranking-read) work as with the ranking written out. Each parameter of the ranking is the call's argument (a value, or a parameter of the query: `q => :q`), else the profile's default, else the query's parameter of the same name.
- **Kept and replicated as writes are**: a profile is logged in its own transaction (numbered where writes are, shipped to replicas), and dropped with its table. A database with a profile needs the format 3 feature `rank_profiles`, which a version of Strata from before profiles refuses by name; once the last profile is dropped and flushed, the database no longer needs it.
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

- **The model** is the file `<models>/<model>/gbdt.json` in the models directory, the one embedded fields' models come from (`--models` on a server, `models=` in Python, else `~/.local/share/strata/models`): LightGBM's `booster.dump_model()` written with `json.dump` (numerical and categorical splits, any missing-value handling, a random forest too), or XGBoost's `booster.save_model("gbdt.json")` (a `gbtree`; categorical splits, multiclass and `dart` are refused). A name is letters, digits, `_`, `-` and `.`. A model is read when a query first uses it and again when its file changes.
- **Its arguments are its features**, in the order it was trained on, each any ranking expression of the row (a score, a column, arithmetic); as many as the model reads. A `NULL` feature is a missing value, which goes where the model sends missing values (LightGBM: `NaN`; or 0 under `zero_as_missing`).
- **The score is the model's raw one**, the sum of its trees, bit for bit what LightGBM's `predict(raw_score=True)` and XGBoost's `predict(output_margin=True)` give (XGBoost's in `f32`, from its base score): `sigmoid(gbdt(…))` is a binary model's probability. A random forest's score is the sum of its trees, as LightGBM's raw score is.
- **Rank it over the top rows of a cheaper ranking.** In a [rerank phase](#rerank-phases) a model is a second phase, which each shard scores on its own rows, one tree at a time over them: a LambdaMART model of 300 trees over BM25's top 100 takes 0.7 to 1.0 ms a query at the median on SciFact, NFCorpus and FiQA, against 0.02 to 0.2 ms for BM25 alone (`strata/results/text/bm25_gbdt.json`, a 4-core cloud VM). `ORDER BY gbdt(bm25(…), columns…)` alone is exact, a [rank-safe boost](#rank-safe-boosts) bounded by the leaves each tree can reach from the text score's bound and each column's range; but summed over hundreds of trees such a bound sits well above what rows score, so the top-k loop skips few matches and scores the rest with every tree: 5.5 to 70 ms on the same collections.
- **A plan names the model and the hash of its file** (`EXPLAIN` shows `ranking model: docs_ltr (hash …)`) and needs the plan feature `rank_models`. A database or shard whose file of that name is another refuses the plan rather than rank with another model; a plan without a hash runs with the file there. A coordinator scores the shards' rows with the model too, so it needs the same file in its own models directory.
- `features()` lists a model's inputs' leaves and then the model's score, and from Python `gbdt("docs_ltr", bm25("title", t), col("rating"))` builds the same ranking.

#### Cross-encoders: `cross_encoder('model', fields…, query)`

A cross-encoder reads the query and a row's text together and scores how well the text answers it. It costs thousands of times what BM25 does per row, so it reranks the top rows of a cheaper ranking, in a rerank phase of its own:

```sql
WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :q) DESC LIMIT 100)
SELECT id, title, score() FROM top
ORDER BY cross_encoder('ms-marco-MiniLM-L-6-v2', title, body, :q) DESC LIMIT 10;

-- mixed with other scores of the row, and a phase after it
WITH top AS (SELECT * FROM docs ORDER BY bm25(body, :q) DESC LIMIT 100),
     ce AS (SELECT * FROM top ORDER BY cross_encoder('bge-reranker-base', body, :q) + 0.1 * coalesce(rating, 0) DESC LIMIT 20)
SELECT id, score() FROM ce ORDER BY bm25(title, :q) + decay(published, now(), 2592000) DESC LIMIT 10;
```

- **The model** is a Hugging Face directory `<models>/<model>` holding `config.json`, `tokenizer.json` and `model.safetensors`: a `BertForSequenceClassification` (such as `cross-encoder/ms-marco-MiniLM-L-6-v2`) or an `XLMRobertaForSequenceClassification` (such as `BAAI/bge-reranker-base`), with one label. It runs through candle in `f32` on the CPU, in a server or Python package built with `--features bert` (`accelerate` on a Mac), as BERT embedding models do; without it, a query by such a model is refused. A model is read when a query first uses it and again when one of its files changes. `builtin:overlap` needs no files: the share of the query's distinct words the text holds, for tests and demos.
- **The text** is the values of the fields named, stored TEXT or KEYWORD fields, joined by a space (`NULL`s left out, each value of a list), and the query is the last argument, a string or a parameter. Query and text together are cut to the model's length (512 tokens at most), the longer of the two first, as Hugging Face's tokenizers cut a pair.
- **The score is the label's logit**, what sentence-transformers' `CrossEncoder.predict` gives with an identity activation (`sigmoid()` of it is the probability for a model trained with one), equal to PyTorch's within 1e-5. A phase may mix it with other scores of the row (`cross_encoder(…) + 0.1 * rating`), and phases after it rerank the rows it kept.
- **It runs once per query, in a global phase**: on a database over the rows the phase before it kept, and through a coordinator once on the coordinator, which reads the text of the rows its shards ranked best in the fetch round of query then fetch and scores them; shards need no model. A cross-encoder is refused as the first ranking (rerank a cheaper ranking's top rows), as a column, and among `features()`.
- **Its cost is per row and per token**: `ms-marco-MiniLM-L-6-v2` takes about 50 ms a row of a few hundred tokens on four cores, so BM25's top 20 takes 0.9 to 1.0 s at the median on SciFact, NFCorpus and ArguAna, and the top 100 4.5 to 5.2 s, where BM25 alone takes 0.04 to 1.1 ms (`strata/results/text/bm25_ce_latency.json`, a 4-core cloud VM). `--max-rerank` on a server (default 100) refuses a phase that would score more rows, and the query's deadline (`--timeout-ms`, 5 s) is checked between batches of 16 rows.
- **Whether it helps depends on the collection**: reranking BM25's top 100 by `ms-marco-MiniLM-L-6-v2`, a model trained on web questions, raises NFCorpus's nDCG@10 from 0.329 to 0.352 and leaves SciFact's where it was (0.687 against 0.691); see [the results](results.md#cross-encoder-reranking). Measure it on your own queries before paying for it.
- **A plan names the model and the hash of its files** (FNV-1a of the three; `EXPLAIN` shows `ranking model: … (hash …)`), with the plan feature `rank_models`, as for [`gbdt`](#ranking-models-gbdtmodel-features). From Python, `.order_by(bm25("body", q)).rerank(cross_encoder("ms-marco-MiniLM-L-6-v2", ["title", "body"], q), 100)` builds the same query.

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

A product's factors are scores (`bm25`, `similarity`, `ppr`, `decay`) and numeric columns. Candidates are the top `depth` (100) rows of each relevance factor (`bm25`, `similarity`, graph scores; of every factor when there is none), scored exactly by every factor; a product of one text score and row values (`bm25(..) * decay(..)`, `bm25(..) * rating`) is a [rank-safe boost](#rank-safe-boosts) and ranks every row the text matches. A factor below 0 (a negative similarity) or missing (a `NULL` column) counts as 0, so the product of two negatives cannot rank high. Over shards, each shard returns its factors' raw scores and the coordinator multiplies them, so a split collection ranks like the whole one.

### A column

`ORDER BY year DESC` orders by a numeric column. Rows without a value come last in either direction (as in Elasticsearch); `ORDER BY year DESC NULLS FIRST` puts them first. Their `score()` is `NULL`. With no `ORDER BY`, rows come in storage order.

## Typos and synonyms

`MATCH` and `bm25` take two options that widen a search beyond the words written:

```sql
SELECT id FROM papers WHERE MATCH(body, 'nueral netwrok', fuzzy => 'auto') LIMIT 10;
SELECT id, score() FROM papers ORDER BY bm25(body, 'nueral netwrok', fuzzy => 1) DESC LIMIT 10;
SELECT id FROM papers WHERE MATCH(body, 'gnn', synonyms => 1) LIMIT 10;
```

- **`fuzzy => 1` or `2`** also matches the indexed terms within that many edits of each word, counting an insertion, deletion, substitution or swap of two neighbouring letters as one edit (optimal string alignment). **`fuzzy => 'auto'`** allows no edits for words of 1 or 2 letters, 1 for 3 to 5 letters and 2 for longer ones. In the `MATCH` grammar, `word~` is `'auto'` for one word and `word~1`, `word~2` choose. A ranked query adds the 50 nearest terms of each word, each weighted `1 − edits / length` (at least 0.1), as Lucene's fuzzy queries do. A word and its typo neighbours share one document frequency, the largest among them, so a rare misspelling scores below the word rather than above it; the expansion is found once per query over every segment's dictionary, so all segments, and the plan line `bm25 expanded with N terms`, agree.
- **`synonyms => 1`** also matches what the server's synonym list says a word or phrase means. The list is in Solr's format: `laptop, notebook, portable computer` makes each find the others, and `tv => television` makes `tv` find `television` but not the reverse. Both sides are analysed like documents, so `laptops` finds `notebook`. A one-word synonym of a word shares that word's document frequency, as Lucene's synonym queries do; the words of a longer synonym score on their own. [strata-server](server.md) loads it from `--synonyms FILE` and edits it at `/v1/synonyms`; from Rust, set `ExecOptions::synonyms`. Without a list, `synonyms => 1` is an error that says so.

## Aggregates

```sql
SELECT category, COUNT(*) FROM docs
WHERE MATCH(body, 'india') AND year >= 2000
GROUP BY category ORDER BY 2 DESC LIMIT 5;

SELECT category, topic, COUNT(*) AS n, AVG(rating), MIN(year), MAX(year), COUNT(DISTINCT source)
FROM docs WHERE year >= 2000
GROUP BY category, topic HAVING n > 20 ORDER BY AVG(rating) DESC LIMIT 10;

SELECT COUNT(*), SUM(clicks), AVG(rating) FROM docs WHERE MATCH(body, 'india');
```

- **`GROUP BY`** takes one or more indexed `KEYWORD`, `INT` or `FLOAT` columns, by name, position or alias. A multi-valued column puts a row in one group per value.
- **Aggregates** are `COUNT(*)`, `COUNT(f)`, `COUNT(DISTINCT f)`, `SUM(f)` and `AVG(f)` over numeric columns, and `MIN(f)` and `MAX(f)` over numeric or keyword columns. Every selected column is a group column or inside an aggregate. Without `GROUP BY` they summarize every row that passes `WHERE`, in one row (`COUNT(*)` 0 and the others `NULL` when none does).
- **`HAVING`** filters groups by their aggregates, written out or by alias, with comparisons, `BETWEEN`, `AND`, `OR` and `NOT`, in three-valued logic: an aggregate of no values (`AVG(rating)` of a group without ratings) is `NULL`, so neither `AVG(rating) < 3` nor `NOT (AVG(rating) < 3)` keeps that group.
- **`ORDER BY`** a group column, an aggregate or a position, with a tie-break; without one, the largest groups come first. `LIMIT` and `OFFSET` apply to groups.

A coordinator over shards merges partial aggregates (sums and counts for `AVG`, the distinct values for `COUNT(DISTINCT)`), so the answers are the same as on one server.

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

- **`IN (SELECT ...)`** and **`NOT IN`** run the subquery first and filter with its values (a multi-valued column contributes each value). **`EXISTS`** is true or false, and a **scalar subquery** must return one column and at most one row. A subquery's values reach the outer query's filter as values, so its indexes serve it: `year > (SELECT AVG(year) ...)` is a range of `year`.
- **`WITH`** results and **`FROM (SELECT ...)`** are read like a table. A `WITH` whose rows are rows of the table (no aggregates) keeps their keys, so an outer query over it reads the table restricted to those rows, in the `WITH`'s order unless it orders them itself, and `(SELECT id FROM top)` can seed a graph. `WITH RECURSIVE` is not supported: `REACHABLE` and `PATH` walk links.
- **Joins** are `JOIN` (inner) and `LEFT JOIN`, of a table with another, with itself or with a `WITH` result; name each side (`papers a JOIN papers b`). `ON` needs an equality between the sides, of two columns of one kind: `KEYWORD`, or `INT` and `FLOAT` compared as numbers (`p.venue = a.id`, an `INT` with a `KEYWORD`, is an error). A multi-valued column joins each of its values. Comparing an `EDGES` field with the key follows its links (`a.cites = b.id`: *b* is a row *a* links to; `b.cites = a.id`: *b* links to *a*); links name keys of their own table, so they join a table with itself. Each `WHERE` or `ON` condition on one side filters that side's scan, and the second side reads only the rows whose join values the first side holds (`b.id IN (...)`, or for links the rows linking to them), before a hash join in memory. Text, vector and graph functions name one side (`MATCH(a.body, ...)`); `ORDER BY` a score ranks by that side's scan, and when the order is the first side's, scans of a growing number of its rows stop once `LIMIT` rows join. Joined columns are named by their expression (`b.title`) unless aliased.
- Over joined or derived rows, `WHERE`, `GROUP BY`, aggregates, `HAVING`, `ORDER BY`, `DISTINCT` and `LIMIT` also take arithmetic, `||`, `LIKE`, `ILIKE`, `CASE`, `COALESCE`, `LOWER`, `UPPER`, `LENGTH`, `ABS`, `ROUND` and `DOT`. So does the `SELECT` list over one table (`SELECT id, rating * 2, dot(emb, :q) FROM papers WHERE ...`): the table's query finds the rows and the columns are computed in memory. Their conditions follow the same three-valued logic as a single table's, multi-valued columns included (`NOT (b.tags = 'red')` keeps rows without tags). Integer arithmetic is exact, and a result too big for an integer is a `FLOAT`; `/` gives a `FLOAT`, and dividing by zero gives `NULL`. `||` skips `NULL`s, and `LIKE` with a `NULL` side is `NULL`. `ORDER BY` puts rows without a value last in either direction unless `NULLS FIRST`, as over a single table.
- Each single-table query, `WITH` result and side of a join holds at most 1,000,000 rows. `UNION`, `INTERSECT`, `EXCEPT`, window functions, and `RIGHT`, `FULL` and `CROSS` joins are not supported.

The executor's plan lines (`--plan`, `/v1/explain`'s `plan`) list each single-table query that ran, with its rows and its own plan.

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
| `ppr(f, seeds, …)` | personalized PageRank, usable wherever a score is, including `rrf` and weighted sums | `alpha 0.15`, `eps 1e-4`, `dir 'both'`, `label`, `max_pushes`, `weighted 0` (`1`: the seed subquery's scores are the restart weights) |
| a `VECTOR` field as `f` | `REACHABLE` and `PATH` follow similarity edges: a row's neighbours are its `k` most similar rows | `k 10`, `min_sim` |

The [query UI](server.md#the-ui)'s Graph tab builds these queries (seeds from keys, a search or a condition; links or similarity; neighbourhood, path or PageRank) and draws the answer.

`through` restricts the rows a traversal may pass through (seeds and end points are exempt); the rest of `WHERE` filters the rows it ends at. `REACHABLE(f, seeds, hops => 0, min_hops => 0)` is the seed set itself, so `NOT` of it removes the seeds from a result. `hops()` refers to the first `REACHABLE` or `PATH` in `WHERE`. The [Graphs](graphs.md) page explains how edges are stored and what these queries cost.

Edges are written as part of their row: a JSON array of target keys, each optionally with a label and a positive weight.

```text
["p2", ["cites", "p7"], ["extends", "p9", 0.5], {"to": "p11", "label": "refutes"}]
```

## Running SQL

The `strata` shell runs one statement, or reads statements from standard input:

```sh
cd strata && cargo build --release
target/release/strata db.strata "SELECT category, COUNT(*) FROM docs WHERE MATCH(body, 'india') GROUP BY category ORDER BY 2 DESC LIMIT 5" --plan
target/release/strata db.strata --param t='rapid covid tests' < queries.sql
```

| flag | effect |
|---|---|
| `--param name=value` | binds `:name`: a number, a string, or a JSON array for a vector |
| `--plan` | prints the plan lines the executor recorded for each statement |
| `--repeat n` | runs each statement *n* times and reports the median time |

From Rust, `Db::query` takes a `SELECT` and a `Params` map, and `Db::sql` takes any statement, on any table, writes and `CREATE TABLE` included. Over the network, [strata-server](server.md) serves the same SQL over every table of a database.

## Tables

A database starts with the table it was created with, and `CREATE TABLE` adds more. Each table has its own columns, primary key, indexes and merges; the tables share the file, its write-ahead log and transactions.

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
) OPTIONS(shard_by = 'usr');
DROP TABLE IF EXISTS wrote, authors;
```

- **Types** are `TEXT`, `KEYWORD`, `INT` (or `INTEGER`, `BIGINT`), `FLOAT` (or `DOUBLE`, `REAL`), `BOOL` (or `BOOLEAN`), `TIMESTAMP` (or `TIMESTAMPTZ`, `TIMESTAMP WITH TIME ZONE`, `DATETIME`; a `DATE` is a `TIMESTAMP` at midnight UTC), `VECTOR(dimensions)`, `SPARSE`, `MEDIA` and `EDGES`; `KEYWORD[]` and `INT[]` hold lists. A `BOOL` or `TIMESTAMP` column holds one value and is neither the primary key nor a reference.
- **`OPTIONS(...)`** takes the options of the [JSON schema](server.md#schemas): `positions`, `stored` and `indexed` (true or false; an indexed `TEXT` or `SPARSE` field is stored unless generated, since merges rebuild its index from the stored values), `from` (the fields a generated `TEXT` field joins), `analyzer` (`'english'`, the default: stopwords dropped and words stemmed, so `running` finds `run`; or `'simple'`: every word kept as written, for names, codes and other languages), `index` (`'hnsw'` with `m` and `ef_construction`, `'flat'`, or `'ivf'` with `nlist`), `codes` (`'sq8'` or `'binary'`), `kind` of a `MEDIA` field, `embed` with `model` for an [embedded vector field](server.md#embedded-vector-fields), which may leave out its dimensions when Strata knows the model's (`VECTOR` alone), and on a `KEYWORD[]` `semantic_id` (the vector field) with `codebook` for a [semantic-ID field](server.md#semantic-ids): `sid KEYWORD[] OPTIONS(semantic_id = 'emb', codebook = 'items-rq')`. Field lists are a string, `'name, bio'`, or `ARRAY['name', 'bio']`.
- **The primary key** is one column: `PRIMARY KEY` on it, or a `PRIMARY KEY (column)` constraint. A table without one only takes plain `INSERT`.
- **`REFERENCES table`**, or `FOREIGN KEY (column) REFERENCES table`, records that a `KEYWORD` or `INT` column holds primary keys of that table, which must exist (or be the table being created) and have a primary key of the column's type. References are not enforced: a row may name a key that is not there, and an inner join along the reference leaves that row out (`LEFT JOIN` keeps it). Joins along them are [ordinary joins](#subqueries-with-and-joins) (`FROM wrote w JOIN authors a ON w.author = a.id`).
- **`NOT NULL`** refuses a row without a value in that column, from any write: SQL, JSON, a file or the Rust API. **`DEFAULT value`** is the value a write that leaves the column out gets: a constant of the column's type on a single-valued `TEXT`, `KEYWORD`, `INT`, `FLOAT`, `BOOL` or `TIMESTAMP` column that is not the key, or `CURRENT_TIMESTAMP` (or `now()`) on a `TIMESTAMP`, the time of the statement, the same in all its rows. An `INSERT` that leaves a column out, a JSON row without its key and a CSV file without its column give it the default; a `NULL` that a write gives stays `NULL`, and a column with `NOT NULL` refuses it. In SQL the keyword `DEFAULT` stands for a column's default, in `VALUES` and in `SET`. The Rust API writes a row as it is given; `Schema::defaults` returns a row of the defaults to start from. The JSON schema's options are `"not_null": true` and `"default"` (`"now"` for the time of the write).
- **`OPTIONS(shard_by = 'column')`** after the columns places the table's rows on [shards](server.md#tables-on-shards) by that column instead of by the primary key, so that a user's events sit on one shard together. The column is a stored, single-valued `KEYWORD` or `INT` with `NOT NULL`, other than the key, and a row keeps it: `UPDATE` does not change it (delete the row and insert it again). In JSON it is `"shard_by": "usr"`. One database places nothing, so the option only matters to a coordinator.
- **`DROP TABLE`** drops tables other than the first. A table that another table references goes with it (`DROP TABLE wrote, authors`) or with `CASCADE`, which removes those references.
- Every other clause (`UNIQUE`, `CHECK`, `ON DELETE`, constraint names, a `DEFAULT` that is an expression, `CREATE TABLE ... AS SELECT`) is an error, and there is no `ALTER TABLE` yet.

`CREATE TABLE` and `DROP TABLE` run through `Db::sql`, Python's in-process [`strata.Database`](python.md), and [strata-server](server.md#schemas)'s `POST /v1/sql`, where a coordinator runs them on every shard.

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
- **`INSERT`** fails on a primary key that already exists. **`INSERT OR REPLACE`** (or `REPLACE INTO`) replaces that row, an upsert. **`ON CONFLICT (key) DO NOTHING`** (or `INSERT OR IGNORE`) writes only the rows whose key is new, and the first of a key the statement repeats. **`ON CONFLICT (key) DO UPDATE SET ...`** inserts the new keys and sets fields of the rows already there, where `excluded.column` is the value the statement gave; a key that comes twice in one such statement is an error. Lists are `ARRAY[…]`; a vector is a parameter, `ARRAY[…]` or a JSON array string; a `SPARSE` value is a JSON object string, and a `MEDIA` value a URI or a JSON object string.
- **`UPDATE`** takes any `WHERE`, and a `LIMIT`. Setting only single-valued `INT` and `FLOAT` fields (`SET clicks = clicks + 1`) changes them in place, without rewriting the row, which suits counters and prices that change often. Setting any other field rewrites the row, which needs every text field stored or generated.
- **`SET`** takes a value or any expression of the row: arithmetic, `||`, `CASE`, `NOT`, comparisons, `lower`, `upper`, `length`, `abs`, `round` and `coalesce` over the row's columns, and in `ON CONFLICT DO UPDATE` over `excluded.column` too. Every expression reads the row as it was, so `SET a = b, b = a` swaps two fields. The result is stored as the field's type: a whole number fits an `INT` (`clicks = price * 2`), but `7 / 2` is 3.5 and an error there (`round(clicks / 2)` fits); one value set to a multi-valued field becomes a list of it. NULL in gives NULL out, as in SQL, except `field + number` and `field - number`, which count a NULL as zero, as a counter does; `coalesce(field, 0) + other` does the same elsewhere. A multi-valued field without values is NULL to `coalesce` and `IS NULL`. Searches, scores and aggregates (`MATCH`, `bm25`, `score()`, `COUNT`) are errors in `SET`. A statement that cannot set one of its rows sets none.
- **`DELETE`** takes any `WHERE`, and `ORDER BY` with `LIMIT` to delete the first rows in that order.
- `RETURNING`, `UPDATE ... FROM`, `DELETE ... USING`, `INSERT ... SELECT`, `ON DUPLICATE KEY UPDATE` and `DO UPDATE ... WHERE` are errors.

Each statement is one transaction, and returns the number of rows it wrote. From Rust, the transaction API (`Database::begin`, `WriteTxn::insert`, `upsert`, `set`, `delete`, `commit`, and `WriteTxn::table` for the other tables) groups several writes into one, across tables too; `WriteTxn::upsert_if_newer` keeps whichever version of a row has the highest version number, for replaying a feed out of order.

## EXPLAIN

`EXPLAIN <statement>` shows the plan of a `SELECT`, `INSERT`, `UPDATE` or `DELETE` without running it: a row per line, in a column named `plan`, the root first and each operator's input indented under it.

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

`EXPLAIN FORMAT JSON` (or `EXPLAIN (FORMAT JSON)`) gives the plan as one row of JSON: fields by their position in the table, typed values (`{"int": 5}`, `{"text": "5"}`), a version and the features the plan needs. Strata refuses a plan from a newer version by name. `EXPLAIN ANALYZE` and other formats are refused.

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

`Join` (or `Left join`) matches its two inputs on the keys after `on`, `targets(f)` being the rows an `EDGES` field links to, with any other `ON` condition after `AND`; `With` names its results, which `Ref` reads; `Key order of top` puts rows in the order of the `WITH` result `top`; `Distinct` keeps each row once. A subquery sits in its condition: `IN values of (...)` (its values), `IN keys of (...)` (its rows' keys), `EXISTS (...)` and a scalar `(...)`, each shown on one line with `;` between its operators. `SELECT DISTINCT` of the table alone is a `GROUP BY`, a single-table query.

Every statement runs as its plan, and a plan in JSON runs on its own: `Db::run` in Rust, `Database.run` in [Python](python.md#querying) and [`POST /v1/plan`](server.md#http-api) over HTTP take one, and check it against the table's schema as the SQL binder checks a statement.

## Reading a plan

`--plan` shows what the planner decided, segment by segment. The same statement can get different plans as the data changes. In the evaluation, an `OR` filter matching about 15% of documents was scanned exactly on SciFact's 5,183 documents, but on the larger collections the planner measured 11% near the query and post-filtered the HNSW result. An RRF query's topic filter matched 11% of Quora globally but only 3.4% near the query, so the planner switched to an exact scan of its 58,767 members, at 3.1 ms. [Planning and execution](index.md#planning-and-execution) describes the choices.
