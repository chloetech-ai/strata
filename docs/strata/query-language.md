---
title: Query language
parent: Strata
nav_order: 1
---

# Strata's query language
{: .no_toc }

Strata speaks standard SQL `SELECT` over one table, with functions for full text, vectors, rank fusion and graphs. Text, structured and vector conditions mix freely in one statement, and the planner decides how to evaluate them together.
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

-- hybrid: reciprocal rank fusion, or a weighted sum of scores
SELECT id FROM papers WHERE year >= 2015
ORDER BY rrf(bm25(body, :text), similarity(emb, :q)) DESC LIMIT 10;

SELECT id FROM papers WHERE year >= 2015
ORDER BY 0.2 * bm25(body, :text) + 0.8 * similarity(emb, :q) DESC LIMIT 10;

-- facets over any result
SELECT category, COUNT(*) FROM papers WHERE MATCH(body, 'vaccine')
GROUP BY category ORDER BY 2 DESC LIMIT 5;
```

## Statement shape

```text
SELECT <columns | * | score() | hops() | COUNT(*)>
FROM <table>
[WHERE <predicate>]
[GROUP BY <keyword column>]
[ORDER BY <score or column> [ASC | DESC] [, <primary key or group column> [ASC | DESC]]]
[LIMIT n [OFFSET m]]
```

- **One table, no joins.** The table name must match the schema's.
- **`SELECT *`** returns every stored, non-vector column.
- **`score()`** is the value of the `ORDER BY` score for each row; **`hops()`** is a row's distance in a graph traversal ([below](#graphs)).
- **`ORDER BY`** takes one key, plus an optional tie-break that must be the primary key (or, in a facet query, the group column). Ranking by a score is always best first; `ORDER BY distance(…) ASC` ranks like `similarity(…) DESC`.
- **Parameters** are `:name` placeholders, bound from the API or from the shell's `--param name=value`. A vector parameter is a JSON array.

## Columns and types

| type | indexed as | filters |
|---|---|---|
| `INT`, `FLOAT` | frame-of-reference column with zone maps, range histogram | `=`, `!=`, `<`, `<=`, `>`, `>=`, `BETWEEN` |
| `KEYWORD` | dictionary codes and one docID list per value | `=`, `!=`, `IN`, `NOT IN`; `GROUP BY` |
| `TEXT` | inverted index (optionally with positions); may be generated from other text fields | `MATCH` |
| `VECTOR(d)` | float32 rows plus 8-bit codes (or 1-bit codes, `"codes": "binary"`), with HNSW, IVF or flat search; may be declared as a model's embedding of other fields ([embedded vector fields](server.md#embedded-vector-fields)) | ordering by `similarity` / `distance`; a similarity graph |
| `SPARSE` | inverted index of learned term weights (SPLADE and similar) | ordering by `similarity` |
| `EDGES` | out-edge CSR and reverse index per segment | `REACHABLE`, `PATH`, `ppr` |

Predicates combine with `AND`, `OR`, `NOT` and parentheses, across all of these.

`KEYWORD` and `INT` fields may be multi-valued (`"multi": true` in the schema): a row holds a list, such as `ARRAY['red', 'blue']`, and a filter matches a row if any of its values matches. A facet counts each distinct value once per row.

A `SPARSE` value is a JSON object of terms and non-negative weights, `{"tok": 1.25, "##en": 0.4}`, as a learned sparse model (SPLADE, BGE-M3's sparse output, OpenSearch's neural sparse models) writes it. Terms are taken as given, not analysed, and weights are stored in steps of 1/1024.

## Full text: `MATCH`

`MATCH(field, query)` is a boolean predicate. The query grammar follows SQLite FTS5:

```text
query   := and ("OR" and)*
and     := unary (["AND"] unary)*          juxtaposition is AND
unary   := ("NOT" | "-") unary | primary    "a NOT b" is "a AND NOT b"
primary := "(" query ")" | "\"phrase\"" | word | word*
```

| query | matches |
|---|---|
| `vaccine mrna` | both words (juxtaposition is `AND`) |
| `vaccine OR vaccination` | either |
| `vaccine -animal`, `vaccine NOT animal` | the first without the second |
| `"messenger rna"` | the phrase (needs positions on the field) |
| `vacc*` | every indexed term with that prefix |
| `vaccine AND (mrna OR "messenger rna") -animal` | any combination, with parentheses |

Operators are uppercase; lowercase `and`, `or` and `not` are ordinary words (and stopwords). Words and phrases are analysed like documents: a word that analyses to several terms (`e-mail`) becomes a phrase, and one that analyses to none (a stopword) is dropped. A prefix is lowercased and folded but not stemmed, and matches the stemmed terms that start with it.

## Scores

### `bm25(field, text)`

BM25 of the analysed terms of `text`, disjunctively: a row scores if it contains any of them. Ranked queries drop English function words (NLTK's 153-word list) and count each term once. Used in `ORDER BY` without a `MATCH`, it is a ranked search; with one, it ranks the matching rows.

| option | default | effect |
|---|---|---|
| `k1`, `b` | 1.2, 0.75 | BM25 parameters |
| `algo` | planner's choice (MaxScore) | `'maxscore'`, `'bmw'` (Block-Max WAND) or `'exhaustive'` |
| `stopwords` | 1 | `0` keeps function words in the query |

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

### `rrf(s1, s2, …)`: reciprocal rank fusion

Each input ranks the rows that pass `WHERE`, to a depth of 100 by default, and a row's fused score is

$$
\mathrm{rrf}(d) = \sum_{i} \frac{1}{k + \mathrm{rank}_i(d)}, \qquad k = 60
$$

Options: `k => 60`, `depth => 100`.

### Weighted sums

Arithmetic on scores gives weighted fusion: `0.2 * bm25(body, :t) + 0.8 * similarity(emb, :q)`. Weights must be numeric constants; `+` and `-` combine any number of scores. Candidates are the union of each input's top 100, re-scored exactly by every input and min-max normalised over that union:

$$
\mathrm{fused}(d) = \sum_i w_i\,\frac{s_i(d) - \min_{C} s_i}{\max_{C} s_i - \min_{C} s_i}
$$

Options, given on any input: `depth => 100` (candidates per input), `norm => 'none'` (keep raw scores), `exact => 1` (score every row that passes `WHERE` instead of the top-100 union). The top-100 shortcut recovers 89–95% of the exact fused top 10 ([results](results.md#hybrid-ranking)).

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

A product's factors are scores (`bm25`, `similarity`, `ppr`, `decay`) and numeric columns. Candidates are the top `depth` (100) rows of each relevance factor (`bm25`, `similarity`, graph scores; of every factor when there is none), scored exactly by every factor. Over shards, each shard returns its factors' raw scores and the coordinator multiplies them, so a split collection ranks like the whole one.

### A column

`ORDER BY year DESC` orders by a numeric column. With no `ORDER BY`, rows come in storage order.

## Facets

```sql
SELECT category, COUNT(*) FROM docs
WHERE MATCH(body, 'india') AND year >= 2000
GROUP BY category ORDER BY 2 DESC LIMIT 5;
```

`GROUP BY` takes one `KEYWORD` column; the select list is that column and `COUNT(*)`. Order by the count (`ORDER BY 2` or `ORDER BY COUNT(*)`) or by the value. `SELECT COUNT(*)` without `GROUP BY` counts the rows that pass `WHERE`.

## Graphs

Rows with an `EDGES` field are nodes of a graph. Graph predicates and scores take **seeds**: a key (`'p1'`), a parameter, a tuple of keys (`('p1', 'p2')`), or a subquery over the same table, whose filters, ranking and `LIMIT` are those of any query.

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

From Rust, `Db::query` takes a `SELECT` and a `Params` map, and `Db::sql` takes any statement, writes included. Over the network, [strata-server](server.md) serves the same SQL.

## Writes

```sql
INSERT INTO products (id, title, price, tags, emb) VALUES ('p1', 'red mug', 12.5, ARRAY['kitchen', 'gift'], :v);
INSERT OR REPLACE INTO products (id, title, price) VALUES ('p1', 'red mug, large', 14.0);
UPDATE products SET stock = stock - 1, price = 11.0 WHERE id = 'p1';
DELETE FROM products WHERE MATCH(title, 'discontinued');
```

- **`INSERT`** fails on a primary key that already exists; **`INSERT OR REPLACE`** replaces that row (an upsert). Lists are `ARRAY[…]`; a vector is a parameter, `ARRAY[…]` or a JSON array string; a `SPARSE` value is a JSON object string.
- **`UPDATE`** takes any `WHERE`. Setting only single-valued `INT` and `FLOAT` fields (`SET clicks = clicks + 1`) changes them in place, without rewriting the row, which suits counters and prices that change often. Setting any other field rewrites the row, which needs every text field stored or generated.
- **`DELETE`** takes any `WHERE`.

Each statement is one transaction, and returns the number of rows it wrote. From Rust, the transaction API (`Database::begin`, `WriteTxn::insert`, `upsert`, `set`, `delete`, `commit`) groups several writes into one; `WriteTxn::upsert_if_newer` keeps whichever version of a row has the highest version number, for replaying a feed out of order.

## Reading a plan

`--plan` shows what the planner decided, segment by segment. The same statement can get different plans as the data changes. In the evaluation, an `OR` filter matching about 15% of documents was scanned exactly on SciFact's 5,183 documents, but on the larger collections the planner measured 11% near the query and post-filtered the HNSW result. An RRF query's topic filter matched 11% of Quora globally but only 3.4% near the query, so the planner switched to an exact scan of its 58,767 members, at 3.1 ms. [Planning and execution](index.md#planning-and-execution) describes the choices.
