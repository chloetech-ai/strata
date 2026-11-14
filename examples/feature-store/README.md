# Feature store demo

A shop that ranks its 240 products for each of 300 shoppers by a model, fed
by features ChloeDB keeps, shown live in the command center (**Apps → Feature
store**). One chloedb-server holds every part: the events, the feature tables,
their history, the training sets, the models and the reads that serve them.

```sh
cd chloedb-server && cargo build --release && cd ..
python3 examples/feature-store/featurestore.py serve --local /tmp/fs-demo
# open http://127.0.0.1:7700/#/app/features
```

It needs Python 3.9 or newer, with no packages. The first start makes 12
minutes of history: shoppers on a random ranking, written at the times they
would have happened. This takes about 10 seconds. From then on, simulated
shoppers ask for a ranking about 12 times a second. Impressions older than
three hours are let go (`--keep MINUTES`, 0 keeps them all), and a model
trains on the last 30 minutes, so a demo left running stays as quick as a new one.

To use a server that is already running, start it with
`--app features=http://127.0.0.1:7810`, then run
`featurestore.py serve --server http://127.0.0.1:7700`. The ASOF JOIN needs a
chloedb-server of release 0.3.1 or later.

![The pipeline](screenshots/pipeline.png)

## What it shows

| tab | what you see |
|---|---|
| Pipeline | Every box, live: impressions per second, the latency of a write, the time from a click until it is in the features, the batch job, the snapshots, the deployed model and its AUC, and the latency of serving. Also the click rate of each model over time, from the impressions table, against the random ranking. |
| Builder | The feature workflows as a map, and an editor for them. Search their statistics with a SQL condition: the steps that match turn red, what they feed turns orange, and what feeds them is outlined in yellow. |
| Shop | The sample application. It shows one shopper's ranking and how much each feature pushed each product up or down. Beside it are the simulator's hidden truth (the categories the shopper likes) and what the features have learned. **Click** writes the 8 impressions and the click in one transaction, reads the shopper's features back, and shows them changed. |
| Features | The feature tables, and how each is kept. Each of the 7 features the model reads, with its definition, its spread over 3,000 (shopper, product) pairs as serving sees them now, its weight in the model, and its cleaning. Any table can be viewed and filtered with a `WHERE`. |
| Training | The two ways to build a training set, side by side: the ASOF JOIN and a join to the latest rows. One shopper's history shows the step from one to the other: their clicks over time, each impression at the value the ASOF JOIN gave it, and the one value the latest rows give them all. Train a model on either, compare their AUCs, and deploy one. Below them, train on any features from the Builder's workflows. |
| Speed | Serving under 1, 4 or 16 clients, and the time to build the training set. Beside them, the benchmark against Feast, read from `chloedb/results/feature-store`. |

![Training](screenshots/training.png)

## How it works

- **Events and labels.** `fs_impressions` has a row for each product a shopper was shown, and `clicked` is the label.
- **Streaming features.** The same `/v1/write` that inserts the impressions also adds to the counters of the shopper, of the product, and of the shopper in that category (`add` operations on `fs_user_online`, `fs_item_online`, `fs_usercat_online`). A click is in the features when its write returns, in about 3 ms here.
- **A batch feature.** Every 10 seconds, a SQL aggregate over the last 2 minutes of impressions writes `fs_item_trend`, each product's recent click rate. Some products turn hot or cold every 3 minutes, so this feature has something to find.
- **Snapshots.** Every 2 seconds, the feature rows that changed are copied into `fs_user_snap`, `fs_item_snap` and `fs_usercat_snap`, each with its time.
- **Transformations.** Smoothed rates, a share, a log, and a price ratio with missing values set to 0 and clipped to ±2. They are written once, in `TRANSFORMS` in `featurestore.py`, and the same code runs for training and for serving.
- **Training sets.** The ASOF JOIN pairs each impression with each snapshot table's latest row from before its time:

  ```sql
  SELECT ... FROM fs_impressions s
  ASOF JOIN fs_user_snap u MATCH_CONDITION (s.ts > u.ts) ON s.usr = u.usr
  ASOF JOIN fs_item_snap i MATCH_CONDITION (s.ts > i.ts) ON s.item = i.item
  ASOF JOIN fs_usercat_snap c MATCH_CONDITION (s.ts > c.ts) ON s.uc = c.uc
  ```

  The leaky alternative is a `LEFT JOIN` to the online tables. It joins each impression to counters that already include its own click.
- **Models.** Logistic regression, trained in pure Python, is stored as a row of `fs_models` with its weights, the SQL of its training set, its timings and three AUCs:
  - on the training rows;
  - on the last minute, held out, with its own join's features;
  - on the last minute with the features serving had at the time.
- **Serving.** The shop reads the shopper's row, the 240 products' two rows each and the shopper's 8 category rows, which is 489 rows in 4 calls to `/v1/kv/get`. It then applies the transformations, scores with the model and keeps the top 8. This takes about 4 to 5 ms here.

On a run on the cloud container (4 cores), the model trained on the ASOF JOIN had an AUC of 0.755. The model trained on the latest rows reported 0.811 on its own held-out rows, but scored 0.727 on what serving actually sees. With the ASOF model deployed, shoppers clicked 23% of what they were shown, against 4.6% for the random ranking.

## Feature workflows (the Builder tab)

The hand-written features above are one way. The Builder makes features
from definitions instead, kept as rows of `fs_workflows`:

- **A workflow** reads one or more inputs, each a table or another
  workflow, joined on a column (`LEFT JOIN p ON a.item = p.id`). It either
  groups by an entity key and aggregates (`count`, `sum`, `avg`, `min`, `max`,
  `count_distinct`), each over all time or the last 1 to 60 minutes, with an
  optional `WHERE`, or it maps each input row. Then it computes expressions
  over those (`(clicks + 1.0) / (views + 10)`, `clamp(...)`, `ln(...)`).
- **Each run** turns the definition into one SQL query, run as of a time
  (`:now`). It writes `fv_<name>`, one row per entity, which serving reads by
  key. It also adds the values that changed to `fv_<name>_hist`, which
  training sets join as of each impression.
- **Chaining.** A workflow that reads another workflow reads its history as of
  the run, so a backfill gives the same values as a live run would have.
  Runs go in dependency order, and a cycle is refused.
- **Statistics.** Each run records each feature's rows, nulls, zeros,
  distinct values, min, max, mean and run time in `fs_workflow_stats`.
  The search box is a `WHERE` over those, plus `age_s`, the seconds since the
  last run. For example, `zeros > rows / 3` finds `user_category.clicks` and
  `user_category_share.share`, then shows what they feed: `wf-v1`.
- **Models.** The Training tab trains on any features of any workflow
  (`ASOF JOIN fv_<name>_hist ... ON s.usr = j.entity`), and the shop serves
  those models with `/v1/kv/get` on `fv_<name>`.

On first start, five workflows are made and backfilled 10 minutes:
`user_activity`, `item_popularity` (impressions joined to the catalog),
`user_category`, `item_signals` (from `item_popularity`) and
`user_category_share` (from `user_category` and `user_activity`). A model on
four of their features had an AUC of 0.762 as served on the cloud container.

![The Builder](screenshots/builder.png)

## Tests

```sh
cd examples/feature-store && python3 -m pytest tests
```

The tests start the demo against `chloedb-server/target/{release,ci}/chloedb-server`, or `CHLOEDB_SERVER_BIN`. They are skipped when the server is not built. They take about 80 seconds, because they wait a minute for live traffic to hold out.
