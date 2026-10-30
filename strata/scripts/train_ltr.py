"""Trains the ranking models of strata-bm25-eval's gbdt variants: for each
collection, a LightGBM LambdaMART model of the features() that
`strata-bm25-eval --features DIR` logged for the queries of a training
split (BM25's top 100 rows of each, with their judgements), written where
gbdt('ltr_<collection>', ...) finds it: <models>/ltr_<collection>/gbdt.json.

    python3 -m venv /tmp/ltr && . /tmp/ltr/bin/activate
    pip install lightgbm==4.7.0
    strata-bm25-eval --dir beir --collections scifact,nfcorpus,fiqa \
        --split train --features /tmp/features --tag ltr-train
    python3 strata/scripts/train_ltr.py /tmp/features /tmp/models

Every collection's model has the same shape (300 trees of up to 31
leaves), trained on one thread with a fixed seed, so a run writes the same
models again. Rows a query's judgements do not name have grade 0.
"""
import json
import os
import sys

import lightgbm as lgb
import numpy as np

PARAMS = {
    "objective": "lambdarank",
    "metric": "ndcg",
    "eval_at": [10],
    "lambdarank_truncation_level": 20,
    "num_leaves": 31,
    "min_data_in_leaf": 50,
    "learning_rate": 0.05,
    "feature_fraction": 1.0,
    "num_threads": 1,
    "deterministic": True,
    "seed": 0,
    "verbose": -1,
}
ROUNDS = 300


def train(path):
    names, groups, X, y = None, [], [], []
    with open(path) as f:
        for line in f:
            q = json.loads(line)
            if names is None:
                names = q["names"]
            assert q["names"] == names, f"{path}: queries log different features"
            if not q["rows"]:
                continue
            groups.append(len(q["rows"]))
            for _id, grade, *features in q["rows"]:
                y.append(grade)
                X.append([np.nan if v is None else v for v in features])
    data = lgb.Dataset(np.array(X, dtype=np.float64), label=np.array(y), group=groups, feature_name=[f"f{i}" for i in range(len(names))])
    booster = lgb.train(PARAMS, data, num_boost_round=ROUNDS)
    return names, booster, len(groups), len(y)


def main():
    features, models = sys.argv[1], sys.argv[2]
    for file in sorted(os.listdir(features)):
        if not file.endswith(".jsonl"):
            continue
        coll = file[: -len(".jsonl")]
        names, booster, queries, rows = train(os.path.join(features, file))
        out = os.path.join(models, f"ltr_{coll}")
        os.makedirs(out, exist_ok=True)
        with open(os.path.join(out, "gbdt.json"), "w") as f:
            json.dump(booster.dump_model(), f)
        print(f"{coll}: {booster.num_trees()} trees on {queries} queries, {rows} rows; features {', '.join(names)}")


if __name__ == "__main__":
    main()
