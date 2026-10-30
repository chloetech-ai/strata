# Next items from a history: `encode()` on MovieLens-1M and Amazon Beauty

`eval.py` trains a sequence model with `strata.train.sequence` (two layers
of causal attention, width 64, item vectors of 64 learned with it), loads
the items and every event into a Strata database, and asks Strata for each
user's next 10 items with one query:

```sql
SELECT id FROM items
WHERE NOT id IN (SELECT item FROM events WHERE usr = :u AND seq < :n)
ORDER BY similarity(emb, encode('m', (SELECT item, ts FROM events
    WHERE usr = :u AND seq < :n ORDER BY seq DESC LIMIT 200), now => :t)) DESC
LIMIT 10
```

(`LIMIT 50` on Beauty; without `ts` and `now =>` when the model reads no
times.)

The protocol is the one published SASRec numbers use: leave-one-out (each
user's last event is the test, the one before it validates, the rest train),
ranked against every item, the user's own items left out. Recall@10 (equal
to HR@10 with one test item) and NDCG@10 are over every user. Each run also
ranks with PyTorch's vectors of the same model, and checks that the first
300 users' Strata answers equal those of the same query given PyTorch's
vector.

Measured on a cloud VM (Intel Xeon @ 2.80GHz, 4 cores, 16.5 GB, Linux 6.18,
PyTorch 2.15 on the CPU), 2026-10-30 and 31. Scores do not depend on the
machine; times do.

## Results

| data | the model reads | Recall@10 | NDCG@10 | PyTorch's | query p50 / p99 | given the vector, p50 | training |
|---|---|---|---|---|---|---|---|
| MovieLens-1M (6,040 users, 3,706 items, 1,000,209 ratings) | items and times | 0.3190 | 0.1877 | equal | 3.14 / 9.86 ms | 0.82 ms | 93 epochs, stopped at 95 min |
| | items only | **0.2783** | **0.1585** | equal | 2.07 / 6.09 ms | 0.68 ms | 94 epochs, stopped at 95 min |
| Amazon Beauty, 5-core (22,363 users, 12,101 items, 198,502 reviews) | items and times | 0.0771 | 0.0473 | equal | 0.73 / 1.94 ms | 0.44 ms | 100 epochs, 55 min |
| | items only | **0.0662** | **0.0406** | equal | 0.62 / 1.50 ms | 0.43 ms | 96 epochs (no gain for 10), 53 min |

Published, same protocol:

| data | model | Recall@10 | NDCG@10 | source |
|---|---|---|---|---|
| MovieLens-1M | SASRec | 0.2500 | 0.1341 | Klenitskiy & Vasilev, RecSys 2023 (arXiv 2309.07602), Table 2 |
| | BERT4Rec | 0.2843 | 0.1537 | same |
| | SASRec+ (SASRec trained with cross-entropy over every item) | 0.3152 | 0.1821 | same |
| Amazon Beauty, 5-core | SASRec | 0.0605 | 0.0318 | TIGER (arXiv 2305.05065), Table 1 |
| | counted transitions (`strata.next_items`, from the items before the last two) | 0.0613 | 0.0398 | [`ngrams-beauty.json`](ngrams-beauty.json), on this data |

The parameters of each run (max length 200 on MovieLens, 50 on Beauty;
every item a negative, temperature 0.05, learning rate 1e-3, dropout 0.2,
batch 128) and every epoch's loss and validation scores are in
[`ml-1m.json`](ml-1m.json), [`ml-1m-no-time.json`](ml-1m-no-time.json),
[`beauty.json`](beauty.json) and [`beauty-no-time.json`](beauty-no-time.json).
`beauty.json` was written before `--no-time` and `--max-minutes` existed;
it ran as their defaults are (with times, no time limit).

## What it says

- **Strata ranks what the model ranks.** On every run Strata's Recall@10
  and NDCG@10 equal those of PyTorch's own vectors, and 300 of 300 users get
  the same top 10 as the query given PyTorch's vector.
- **The like-for-like rows are "items only"**: SASRec reads no times. There
  the model is above published SASRec on both collections (by 0.028
  Recall@10 and 0.024 NDCG@10 on MovieLens, 0.006 and 0.009 on Beauty),
  next to BERT4Rec on MovieLens, and below SASRec+ there. On Beauty it is
  0.005 Recall@10 above counted transitions, with about their NDCG@10. Both MovieLens runs were stopped by the 95-minute limit with
  the validation score still rising (best epoch 91 of 93, and 94 of 94), so
  they are a floor of this model's score, not its best.
- **Times help** (0.041 Recall@10 on MovieLens, 0.011 on Beauty): with
  them the model reads the time from each event to the next, and from the
  newest to the request's `now` (here the test event's time). On Beauty
  part of that is the data's: 43% of users review their last two items on
  the same day, and the reviews of one day are in the file's order, by
  product, which counted transitions also pick up.
- **The query costs** the model plus reading the history and the vector
  search: 0.62 to 0.73 ms at p50 on Beauty's 50 events, of which 0.43 to
  0.44 ms is the same query given the vector, and 2.1 to 3.1 ms on MovieLens's 200.

## The model alone

`strata-encode-bench` (`strata/src/bin/strata-encode-bench.rs`) runs a
model over 2,000 random histories of each length on one thread
([`encode-bench.txt`](encode-bench.txt)), on the MovieLens model:

| events | p50 | p99 |
|---|---|---|
| 1 | 0.014 ms | 0.049 ms |
| 10 | 0.091 ms | 0.20 ms |
| 50 | 0.28 ms | 0.50 ms |
| 100 | 0.62 ms | 1.04 ms |
| 200 | 1.48 ms | 2.23 ms |

Fifty events through the two layers take 0.28 ms; the cost grows a little
faster than the length, as attention does.

## Running it

```bash
# MovieLens-1M: https://files.grouplens.org/datasets/movielens/ml-1m.zip (ratings.dat)
# Beauty: http://snap.stanford.edu/data/amazon/productGraph/categoryFiles/reviews_Beauty_5.json.gz
pip install torch && (cd strata-py && maturin develop --release)
python eval.py --data ml-1m --dir ~/data/seq/ml-1m --out . --max-minutes 95
python eval.py --data ml-1m --dir ~/data/seq/ml-1m --out . --max-minutes 95 --no-time --tag=-no-time
python eval.py --data beauty --dir ~/data/seq/beauty --out . --max-len 50
python eval.py --data beauty --dir ~/data/seq/beauty --out . --max-len 50 --no-time --tag=-no-time
cargo run --release --bin strata-encode-bench -- <work>/models/ml1m
```
