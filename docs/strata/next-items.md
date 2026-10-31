---
title: Next items
parent: Strata
nav_order: 7
---

# Next items
{: .no_toc }

What a user will open, watch or buy next, from what came after the same items before: counts kept in an ordinary table as events arrive, read with backoff from the last two items to the last one. They update with every event, explain themselves, and are the baseline that a learned sequence model has to beat.
{: .fs-5 .fw-300 }

- TOC
{:toc}

## Counts in a table

Each row counts how often one item came right after one context: the last item, or the last two. The key is the context's items and then the next item, so with `cluster_by = 'id'` a context's rows sit together.

```sql
CREATE TABLE next (
  id KEYWORD PRIMARY KEY,   -- the context's items, then the next item
  ctx KEYWORD NOT NULL,     -- the last item, or the last two
  nxt KEYWORD NOT NULL,
  n FLOAT DEFAULT 0         -- how often (or a decayed weight)
) OPTIONS(cluster_by = 'id');
```

In Python, `strata.ngrams` writes the statement (`ngrams.create_table()`) or the schema (`ngrams.schema()`, for `strata.open` or `strata-server --schema`), the writes of an event and the read:

```python
import strata
from strata import ngrams

db = strata.open("next.strata", ngrams.schema())

# after each event: item "c" after the user's history [..., "a", "b"]
db.write(ngrams.ops(["a", "b"], "c"))

strata.next_items(db, ["a", "b"], k=10)   # [("c", 1.0), ...]
```

`ngrams.ops(history, item)` returns one `add` per order, each of which creates its row on the context's first event, so an event costs two writes; `order=3` also counts the last three items. They are the operations strata-server's `/v1/write` takes ([writes](server.md#writes)), so `Client.write` sends them and `Database.write` applies them in-process. Items are strings without the two separators the keys use (`\x1f` between a context's items, `\x1e` before the next item).

Counts can decay in place, so that recent behaviour weighs more: `UPDATE next SET n = n * 0.9` rewrites no row, and a row whose weight reaches 0 is no longer read.

## Reading with backoff

`next_items(db, history, k=10, order=2, backoff=0.4)` scores each item by stupid backoff ([Brants et al., 2007](https://aclanthology.org/D07-1090/)): an item seen after the last two items scores its share of that context's weight; one seen only after the last item scores its share there times 0.4; a pair never seen backs off the same way. It returns `(item, score)` pairs, best first, ties by item.

The answer is the same as scoring every item of both contexts, but it reads less. Per context it runs a sum and then reads rows best first, a page at a time, and stops at the first row that cannot reach the top `k`. Then it checks that the items it takes from the last item's context have no row in the pair's context, by their keys. Over a server it is the same SQL, which any language can run:

```sql
SELECT SUM(n) FROM next WHERE ctx = :c AND n > 0;
SELECT nxt, n FROM next WHERE ctx = :c AND n > 0 ORDER BY n DESC, id LIMIT 64 OFFSET 0;   -- ties by key: by item
SELECT nxt FROM next WHERE id IN (...) AND n > 0;   -- the candidates' keys under the longer context
```

## Measured

On Amazon Beauty (22,363 users' reviews of 12,101 products in time order, the 5-core set sequential recommenders report on), each user's last item predicted from the items before it, ranked among all items, with the counts in-process on one thread of a 4-core cloud VM (Intel Xeon @ 2.10GHz):

| counts from | order | Recall@10 | NDCG@10 | p50 ms | p99 ms |
|---|---:|---:|---:|---:|---:|
| every transition before each user's last item | 1 | 0.0688 | 0.0459 | 0.57 | 1.46 |
| | 2 | 0.0694 | 0.0450 | 0.64 | 1.69 |
| only those before the last two, as SASRec trains | 1 | 0.0607 | 0.0404 | 0.49 | 1.23 |
| | 2 | 0.0613 | 0.0398 | 0.57 | 1.38 |

SASRec's published numbers on the same split are 0.0605 and 0.0318 (as [TIGER](https://arxiv.org/abs/2305.05065) reports them, from P5), so counted the way SASRec trains, counts match its recall and beat its NDCG: a strong baseline on this data. Two caveats: 43.5% of users wrote their last two reviews on the same day, which the usual preprocessing leaves in product order and which favours counted transitions; and with under nine reviews per user, the pair's context adds little over the last item's (order 3 adds nothing). Answers were the same as scoring every item for the first 2,000 users. Fewer than 10 scored items were filled with the most frequent ones. Script and results: [`results/sequence/ngrams_beauty.py` and `ngrams-beauty.json`](https://github.com/rkonow/strata/tree/main/strata/results/sequence).

A sequence model ([`encode()`](query-language.md#sequence-models-encodemodel-history)) trained on the same split, reading items only, has 0.0662 and 0.0406 ([results](https://github.com/rkonow/strata/tree/main/strata/results/sequence)), at 0.6 ms a query.

## Several steps ahead

Counts predict the next item. For items further ahead, or for "seen together", the same events can feed an [edge table](query-language.md#edge-tables), whose edges are directed: adding 1/*g* to the edge from the item *g* steps back to the new one records what comes after what, and `ppr(cooc, :x, dir => 'out')` walks forward from the last items. `dir => 'both'`, `ppr`'s default, reads the same edges as seen together.

```sql
SELECT id FROM items
ORDER BY ppr(cooc, (SELECT item FROM events WHERE usr = :u ORDER BY ts DESC LIMIT 5), dir => 'out', eps => 1e-6) DESC
LIMIT 20;
```

Personalized PageRank pushes a row only while its share of the walk is at least `eps` (1e-4 by default) times the sum of its edges' weights, so a single seed whose edges add up to more than 10,000 is never pushed and the walk returns nothing. Pass a smaller `eps`, as above, or keep the weights decayed or normalised.

## Which to use

Counts are exact and live: an event changes the next answer, and each suggestion is explained by a count. They know only the last one or two items and nothing about a context never seen, where they fall back to the last item alone. A walk over directed edges reaches several steps ahead and related items, at five writes per event. Learned sequence models, which read long histories and combine what they have not seen together, are the next step; these counts are the baseline they have to beat.
