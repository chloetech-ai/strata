# Generative retrieval with semantic IDs, against `encode()`, walks and their fusion

[`tiger.py`](tiger.py) asks for each user's next 10 items in four ways, on
the three Amazon 5-core collections that generative retrieval is reported
on (Beauty, Sports and Outdoors, Toys and Games):

- **Generated.** A TIGER-style encoder-decoder
  ([Rajput et al., NeurIPS 2023](https://arxiv.org/abs/2305.05065)),
  trained with PyTorch, reads the user's history as semantic IDs and writes
  the next item's ID a number at a time, by a beam search of 20. At each
  step it asks Strata which numbers can come next among the items the user
  does not have, and its top 10 beams are its answer:

  ```python
  db.sid_next(["", "12", "12.7"], field="sid", where="NOT id IN ('B00...', ...)", table="items")
  ```
- **The sequence encoder** of [`encode()`](../sequence/README.md) (PR
  A8), in one query:

  ```sql
  SELECT id FROM items
  WHERE NOT id IN (SELECT item FROM events WHERE usr = :u AND seq < :n)
  ORDER BY similarity(iv, encode('beauty-seq', (SELECT item FROM events
      WHERE usr = :u AND seq < :n ORDER BY seq DESC LIMIT 50))) DESC
  LIMIT 10
  ```
- **An item-to-item walk**: personalized PageRank from the user's last 5
  items over an [edge table](../../../docs/strata/next-items.md#several-steps-ahead)
  whose edge from the item *g* steps back to the next one weighs 1/*g*
  (*g* up to 5), `ORDER BY ppr(cooc, (SELECT item ... LIMIT 5), dir => 'out', eps => 1e-6) DESC`.
- **Their fusion**, in the same query: `rrf` of the encoder's and the
  walk's scores and of the decoder's answer, passed in as `ranked(:gen)`.

The items, events and edges are one Strata database per collection. Each
item's text (title, brand, categories and price, as TIGER embeds them) is
embedded by bge-small-en-v1.5 in an embedded vector field, and a codebook
of 3 levels of 256 trained on those vectors (`strata.train.codebook`) gives
each item a semantic ID of 4 numbers, the 3 levels and a tie, in a
semantic-ID field.

The protocol is TIGER's and SASRec's: leave-one-out (each user's last item
is the test, the one before it validates, the rest train), every user,
ranked against every item. Every method learns from the transitions before
each user's last two items, reads the whole history before the test item,
and leaves out the items the user has. Recall@k (HR@k, with one test item)
and NDCG@k are over every user. With 19,000 to 36,000 users, one standard
error of a method's Recall@10 is 0.0010 to 0.0017, so differences under
about 0.003 are within noise.

Measured on a cloud VM (Intel Xeon @ 2.80GHz, 4 cores, 16.5 GB, Linux
6.18, PyTorch 2.15 on the CPU), with Strata and strata-py at 62458e8,
2026-10-31. Scores do not depend on the machine; times do. Times are of
one request at a time over the first 300 users: for the decoder, its beam
search with every `sid_next` call; for the others, the query.

## Results

Best of each column in bold. Published numbers: TIGER, Table 1.

### Amazon Beauty

22,363 users, 12,101 items, 198,502 reviews.

| method | Recall@5 | NDCG@5 | Recall@10 | NDCG@10 | time p50 / p99 |
|---|---:|---:|---:|---:|---:|
| generated: the decoder, each step's numbers from `sid_next` | 0.0441 | 0.0294 | 0.0667 | 0.0367 | 31 / 45 ms |
| sequence encoder (`encode()`, A8) | 0.0486 | 0.0357 | 0.0656 | 0.0412 | 0.90 / 1.55 ms |
| item-to-item walk (`ppr`) | 0.0523 | 0.0364 | 0.0758 | 0.0439 | 72 / 110 ms |
| generated + A8, `rrf` | 0.0533 | 0.0369 | 0.0749 | 0.0438 | the decoder's, plus 0.99 / 1.79 ms |
| A8 + walk, `rrf` | **0.0611** | **0.0440** | 0.0834 | **0.0511** | 77 / 126 ms |
| generated + A8 + walk, `rrf` | 0.0611 | 0.0425 | **0.0855** | 0.0503 | the decoder's, plus 74 / 121 ms |
| published TIGER | 0.0454 | 0.0321 | 0.0648 | 0.0384 | |
| published SASRec | 0.0387 | 0.0249 | 0.0605 | 0.0318 | |

### Sports and Outdoors

35,598 users, 18,357 items, 296,337 reviews.

| method | Recall@5 | NDCG@5 | Recall@10 | NDCG@10 | time p50 / p99 |
|---|---:|---:|---:|---:|---:|
| generated: the decoder, each step's numbers from `sid_next` | 0.0238 | 0.0157 | 0.0377 | 0.0202 | 48 / 61 ms |
| sequence encoder (`encode()`, A8) | 0.0224 | 0.0160 | 0.0320 | 0.0190 | 1.52 / 2.68 ms |
| item-to-item walk (`ppr`) | 0.0279 | 0.0190 | 0.0414 | 0.0233 | 241 / 339 ms |
| generated + A8, `rrf` | 0.0279 | 0.0187 | 0.0422 | 0.0233 | the decoder's, plus 1.33 / 2.50 ms |
| A8 + walk, `rrf` | **0.0349** | **0.0249** | 0.0486 | 0.0293 | 175 / 271 ms |
| generated + A8 + walk, `rrf` | 0.0347 | 0.0235 | **0.0539** | **0.0297** | the decoder's, plus 161 / 247 ms |
| published TIGER | 0.0264 | 0.0181 | 0.0400 | 0.0225 | |
| published SASRec | 0.0233 | 0.0154 | 0.0350 | 0.0192 | |

### Toys and Games

19,412 users, 11,924 items, 167,597 reviews.

| method | Recall@5 | NDCG@5 | Recall@10 | NDCG@10 | time p50 / p99 |
|---|---:|---:|---:|---:|---:|
| generated: the decoder, each step's numbers from `sid_next` | 0.0421 | 0.0291 | 0.0627 | 0.0357 | 32 / 51 ms |
| sequence encoder (`encode()`, A8) | 0.0501 | 0.0376 | 0.0624 | 0.0416 | 0.85 / 1.60 ms |
| item-to-item walk (`ppr`) | 0.0584 | 0.0413 | 0.0844 | 0.0497 | 97 / 162 ms |
| generated + A8, `rrf` | 0.0559 | 0.0401 | 0.0776 | 0.0471 | the decoder's, plus 1.09 / 2.59 ms |
| A8 + walk, `rrf` | 0.0687 | 0.0514 | 0.0916 | 0.0588 | 94 / 152 ms |
| generated + A8 + walk, `rrf` | **0.0715** | **0.0517** | **0.0986** | **0.0604** | the decoder's, plus 97 / 144 ms |
| published TIGER | 0.0521 | 0.0371 | 0.0712 | 0.0432 | |
| published SASRec | 0.0463 | 0.0306 | 0.0675 | 0.0374 | |

Every epoch's loss and validation scores, the codebooks' statistics and
the queries are in [`beauty.json`](beauty.json), [`sports.json`](sports.json)
and [`toys.json`](toys.json).

## What it says

- **The decoder is near published TIGER on two of the three.** Recall@10
  is 0.0019 above TIGER's on Beauty, 0.0023 below on Sports and 0.0085
  below on Toys (NDCG@10 0.0017, 0.0023 and 0.0075 below), with a quarter
  of TIGER's parameters and about an hour of training on 4 CPU cores.
- **Strata's next codes constrain it exactly.** On each collection they
  equal those of a trie of the same codes, without the user's items, on
  1,280 of 1,280 prefixes checked, and every user got 10 items.
- **Alone, it does no better than A8.** Its Recall@10 is A8's within noise
  on Beauty and Toys (+0.0011 and +0.0003) and above it on Sports
  (+0.0057), but its NDCG@10 and Recall@5 are lower on Beauty and Toys
  (NDCG@10 0.0046 and 0.0059 below): the beam finds as many of the next
  items, and ranks them lower. A8's query takes 0.85 to 1.5 ms; the
  decoder, 31 to 48 ms.
- **Together they beat each.** `rrf` of A8 and the decoder's answer is
  above the better of the two on every collection, by 0.0082, 0.0045 and
  0.0149 Recall@10 and 0.0026, 0.0031 and 0.0055 NDCG@10: they find
  different items.
- **The walk beats both.** Alone it is above both learned models on every
  collection (by 0.0091, 0.0037 and 0.0217 Recall@10), and A8 with it is
  above it again (by 0.0072 to 0.0076). Adding the decoder to those two
  gives the best Recall@10 on all three (by 0.0020, 0.0053 and 0.0070 more)
  and the best of every column on Toys, but not the best Recall@5 or
  NDCG@5 on Beauty and Sports, nor NDCG@10 on Beauty. Part of the walk's
  lead is the data's: 43.5%, 45.1% and 49.0% of users reviewed their last
  two items on the same day, and the reviews of a day are in the file's
  order, by product, which transitions pick up.
- **Where the time goes.** A decoder request (one user, a beam of 20,
  4 steps) is 90% the model on the CPU and 10% Strata's next codes: 3.1 to
  5.0 ms for its 4 calls. The walk, at `eps => 1e-6`, takes 72 to 241 ms,
  and is what the best fusions wait on; its times vary by about 15% from
  one pass over the same users to the next.
- **So the decoder stays in Python.** The design moves it into Strata only
  if it wins and its latency over HTTP is the bottleneck. It does not win
  alone, and Strata's part of its time is a tenth. Its answer helps as
  `ranked(:gen)` in a fused query. Two things could change the picture:
  TIGER's full recipe on a GPU (a model four times the size and 200,000
  steps), which may close the gap on Toys; and a cheaper walk.

## The decoder, and how it differs from TIGER's

TIGER's architecture, made to train on a CPU:

- **The input** is a user token (the user's id hashed into 2,000, as
  TIGER) and the last 20 items. TIGER gives each number of a code a
  position of its own (81 positions); here a position is an item, the sum
  of its four numbers' embeddings (21 positions). TIGER's layout
  (`--tiger-tokens code`) took about 7 times as long an epoch, an
  estimated 25 minutes on Beauty against 3.5.
- **The encoder is causal**: each position reads the ones before it, so
  one pass over a window of 21 items trains each of its 20 targets on the
  items before that target (the decoder's cross-attention is masked to
  them). TIGER's encoder reads all of its input, one target a sample.
- **The size.** 4 encoder and 4 decoder layers and a width of 128, as
  TIGER; 4 heads (TIGER: 6 of 64), feed-forward 1,024, dropout 0.1, and an
  output head of 256 for each of the 4 numbers: 3.4 million parameters,
  against TIGER's 13 million.
- **Training.** AdamW at 1e-3 (weight decay 0.01), 1,000 warm-up steps
  then inverse square root, gradients clipped at norm 1, 48 windows a step
  (250 to 270 targets). It stopped when Recall@10 of the constrained beam on
  2,000 validation users had not risen for 5 epochs: best epoch 10 of 15
  on Beauty (52 minutes), 11 of 16 on Sports (81), 14 of 19 on Toys (57),
  7,400 to 12,200 steps. TIGER trained 200,000 steps of 256 (100,000 on
  Toys), at 0.01, on a GPU.
- **The codes.** TIGER embeds the same text with Sentence-T5 (768
  dimensions) and trains an RQ-VAE; here bge-small-en-v1.5 (384) and
  residual k-means. Each level uses all 256 numbers; 10,216 to 14,250
  distinct 3-level codes, with up to 11 to 39 items sharing one (the tie
  tells them apart).
- **The beam** keeps 20, and each step's numbers are those `sid_next`
  gives, so every code it writes is an item the user does not have. TIGER
  counts the codes that are no item instead.

A8 is trained as in [`../sequence`](../sequence/README.md) (two layers,
width 64, item vectors of 64 learned from random ones, every item a
negative, temperature 0.05, the last 50 items, no times), stopped when
validation NDCG@10 had not risen for 10 epochs or at a time limit: on
Beauty at 60 minutes (99 epochs, still rising slowly: Recall@10 0.0862 at
epoch 91, 0.0873 at 99), on Sports at 100 (best epoch 85 of 93), on Toys
at no gain (best 138 of 148, 70 minutes).

## Running it

```bash
# http://snap.stanford.edu/data/amazon/productGraph/categoryFiles/
#   reviews_Beauty_5.json.gz and meta_Beauty.json.gz, and the same of
#   Sports_and_Outdoors and Toys_and_Games, each collection in a directory
pip install torch && (cd strata-py && maturin develop --release --features bert)
STRATA_MODELS=~/data/seq/models strata-server fetch-model bge-small-en-v1.5
python tiger.py --data beauty --dir ~/data/seq/beauty --work ~/work/beauty --models ~/data/seq/models --stage prep
python tiger.py --data beauty --dir ~/data/seq/beauty --work ~/work/beauty --models ~/data/seq/models --stage tiger --max-minutes 100
python tiger.py --data beauty --dir ~/data/seq/beauty --work ~/work/beauty --models ~/data/seq/models --stage a8 --max-minutes 60
python tiger.py --data beauty --dir ~/data/seq/beauty --work ~/work/beauty --models ~/data/seq/models --stage eval --out .
```

(`--max-minutes 100` for A8 on Sports and Toys; the decoder stopped on its
own before its limit on all three.) Each stage is a run of
its own: here one took up to 100 minutes. `eval` keeps each part's
answers under `<work>/eval-cache`, so a run that was stopped resumes, and
runs the queries over every user on every core.
