"""Writes the GBDT models of this directory and reference.json: each
model's predictions, by LightGBM and XGBoost themselves, of rows chosen to
cross every split on both sides, at the thresholds, and missing.

    python3 -m venv /tmp/gbdt && . /tmp/gbdt/bin/activate
    pip install lightgbm==4.7.0 xgboost-cpu==3.4.1
    python3 make.py

Features: 0 a text score (>= 0), 1 a rating with missing values, 2 a
category (0 to 5). tests/rank_models.rs reads the models and checks
Strata's gbdt() against reference.json bit for bit.
"""
import json
import os

import lightgbm as lgb
import numpy as np
import xgboost as xgb

HERE = os.path.dirname(os.path.abspath(__file__))
rng = np.random.default_rng(20261030)

n = 600
X = np.column_stack([
    rng.gamma(2.0, 3.0, size=n),
    np.round(rng.uniform(0, 5, size=n), 1),
    rng.integers(0, 6, size=n).astype(float),
])
X[rng.random(n) < 0.15, 1] = np.nan
X[rng.random(n) < 0.1, 1] = 0.0
signal = 0.4 * X[:, 0] + np.nan_to_num(X[:, 1], nan=2.0) + np.isin(X[:, 2], [1, 4]) * 3.0
groups = [30] * (n // 30)
labels = np.clip(np.round(signal / 4 + rng.normal(size=n) * 0.5), 0, 4)


def save(name, text):
    os.makedirs(os.path.join(HERE, name), exist_ok=True)
    with open(os.path.join(HERE, name, "gbdt.json"), "w") as f:
        f.write(text)


models = {}

common = {"verbose": -1, "num_leaves": 7, "min_data_in_leaf": 5, "min_data_per_group": 5, "cat_smooth": 1, "seed": 7, "deterministic": True}
d = lgb.Dataset(X, labels, group=groups, categorical_feature=[2], free_raw_data=False)
models["lgb_rank"] = lgb.train({**common, "objective": "lambdarank"}, d, num_boost_round=20)
models["lgb_zero"] = lgb.train({**common, "objective": "regression", "zero_as_missing": True}, lgb.Dataset(X, signal, categorical_feature=[2]), num_boost_round=12)
models["lgb_rf"] = lgb.train({**common, "objective": "regression", "boosting": "rf", "bagging_fraction": 0.7, "bagging_freq": 1, "feature_fraction": 0.9}, lgb.Dataset(X, signal), num_boost_round=8)
for name in ["lgb_rank", "lgb_zero", "lgb_rf"]:
    save(name, json.dumps(models[name].dump_model()))

xgb_params = {"max_depth": 3, "eta": 0.3, "seed": 7}
dx = xgb.DMatrix(X, label=labels, missing=np.nan)
dx.set_group(groups)
models["xgb_rank"] = xgb.train({**xgb_params, "objective": "rank:ndcg"}, dx, num_boost_round=20)
models["xgb_logistic"] = xgb.train({**xgb_params, "objective": "binary:logistic"}, xgb.DMatrix(X, label=(signal > 6).astype(float), missing=np.nan), num_boost_round=15)
models["xgb_reg"] = xgb.train({**xgb_params, "objective": "reg:squarederror"}, xgb.DMatrix(X, label=signal, missing=np.nan), num_boost_round=15)
for name in ["xgb_rank", "xgb_logistic", "xgb_reg"]:
    path = os.path.join(HERE, name, "gbdt.json")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    models[name].save_model(path)


def thresholds(name):
    """The numerical thresholds of a model's splits, per feature."""
    out = {0: set(), 1: set(), 2: set()}
    if name.startswith("lgb"):
        def walk(t):
            if "split_feature" in t and t["decision_type"] == "<=":
                out[t["split_feature"]].add(float(t["threshold"]))
            for k in ("left_child", "right_child"):
                if k in t:
                    walk(t[k])
        for info in models[name].dump_model()["tree_info"]:
            walk(info["tree_structure"])
    else:
        for tree in json.load(open(os.path.join(HERE, name, "gbdt.json")))["learner"]["gradient_booster"]["model"]["trees"]:
            for i, l in enumerate(tree["left_children"]):
                if l != -1:
                    out[tree["split_indices"][i]].add(float(np.float32(tree["split_conditions"][i])))
    return out


# Rows: the training rows, the thresholds themselves and their float32
# neighbours, zeros, missing values and categories out of range.
rows = [list(r) for r in X[:200]]
for name in models:
    for f, ts in thresholds(name).items():
        for t in sorted(ts):
            for v in (t, float(np.nextafter(np.float32(t), np.float32(-np.inf))), float(np.nextafter(t, np.inf))):
                r = list(X[len(rows) % n])
                r[f] = v
                rows.append(r)
specials = [np.nan, 0.0, -0.0, 1e-36, -1e-36, -0.5, -1.0, 7.0, 2.9, 1e30]
for v in specials:
    for f in range(3):
        r = list(X[len(rows) % n])
        r[f] = v
        rows.append(r)
R = np.array(rows, dtype=float)

reference = {"rows": [[None if np.isnan(v) else v for v in r] for r in R.tolist()], "predictions": {}}
for name, m in models.items():
    if name.startswith("lgb"):
        p = m.predict(R, raw_score=True)
    else:
        p = m.predict(xgb.DMatrix(R, missing=np.nan), output_margin=True)
    reference["predictions"][name] = [float(x) for x in p]
with open(os.path.join(HERE, "reference.json"), "w") as f:
    json.dump(reference, f)
print(len(rows), "rows;", {k: os.path.getsize(os.path.join(HERE, k, "gbdt.json")) for k in models})
