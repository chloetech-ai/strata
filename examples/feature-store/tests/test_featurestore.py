"""The feature store demo end to end, against a chloedb-server built from this
checkout (skipped when there is none): history made, a point-in-time training
set with nothing from the future, a leaky one that looks better than it is,
a model deployed and serving, a click in the features at once.

    cd examples/feature-store && python3 -m pytest tests
"""
import json
import os
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(HERE))
import featurestore as fs  # noqa: E402


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def server_bin():
    if os.environ.get("CHLOEDB_SERVER_BIN"):
        return os.environ["CHLOEDB_SERVER_BIN"]
    for prof in ("release", "ci", "debug"):
        p = HERE.parent.parent / "chloedb-server" / "target" / prof / "chloedb-server"
        if p.exists():
            return str(p)
    return None


def test_auc_and_transforms():
    assert fs.auc([0.1, 0.4, 0.35, 0.8], [0, 0, 1, 1]) == 0.75
    assert fs.auc([0.5, 0.5], [0, 1]) == 0.5
    raw = {k: None for k in fs.RAW_COLS}
    raw["price"] = 10.0
    x = fs.transform(raw)  # a new shopper and product: nothing missing breaks it
    assert len(x) == len(fs.FEATURES) and x[fs.FEATURES.index("price_ratio")] == 0.0
    raw.update(u_clicks=2, u_click_price=2.0)  # clicked $1 on average: clipped
    assert fs.transform(raw)[fs.FEATURES.index("price_ratio")] == 2.0


@pytest.fixture(scope="module")
def demo(tmp_path_factory):
    b = server_bin()
    if not b:
        pytest.skip("no chloedb-server built (cd chloedb-server && cargo build --release)")
    d = tmp_path_factory.mktemp("fs")
    port, app = free_port(), free_port()
    p = subprocess.Popen([sys.executable, str(HERE / "featurestore.py"), "serve", "--local", str(d), "--port", str(port),
                          "--listen", f"127.0.0.1:{app}", "--server-bin", b, "--backfill", "4", "--rate", "20"],
                         stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    url = f"http://127.0.0.1:{port}/v1/apps/features"
    end = time.time() + 120
    while time.time() < end:
        line = p.stdout.readline()
        if line.startswith("ready"):
            break
        if p.poll() is not None:
            raise RuntimeError("the demo stopped: " + line)
    else:
        raise RuntimeError("the demo did not start")

    def call(path, body=None):
        req = urllib.request.Request(url + path, None if body is None else json.dumps(body).encode(), {"content-type": "application/json"})
        with urllib.request.urlopen(req, timeout=300) as r:
            return json.loads(r.read())

    time.sleep(70)  # a minute of live traffic to hold out
    yield call
    p.terminate()
    p.wait(10)


def test_the_app_is_listed_and_serves_its_view(demo):
    m = demo("/app.json")
    assert m["name"] == "features" and m["view"] == "ui/view.js"


def test_point_in_time_and_leakage(demo):
    asof = demo("/api/train", {})
    leaky = demo("/api/train", {"leaky": True})
    assert asof["train_rows"] > 1000
    # The leaky model reports more than it is worth; the ASOF one does not.
    assert leaky["auc_holdout"] > leaky["auc_true"] + 0.03
    assert abs(asof["auc_holdout"] - asof["auc_true"]) < 1e-9
    assert asof["auc_true"] > 0.6
    # No feature row from after its impression.
    tl = demo("/api/timeline?user=u005")
    for r in tl["impressions"]:
        assert r["u_ts"] is None or r["u_ts"] < r["ts"]
    models = demo("/api/models")["models"]
    assert {m["id"] for m in models} >= {asof["id"], leaky["id"]}

    demo("/api/deploy", {"id": asof["id"]})
    r = demo("/api/shop?user=u010")
    assert r["model"] == asof["id"] and len(r["items"]) == fs.K
    scores = [e["score"] for e in r["items"]]
    assert scores == sorted(scores, reverse=True)

    # A click: in the shopper's features as soon as the write returns.
    c = demo("/api/click", {"user": "u010", "shown": [e["id"] for e in r["items"]], "item": r["items"][0]["id"], "model": r["model"]})
    assert c["after"]["clicks"] == (c["before"] or {}).get("clicks", 0) + 1
    assert c["after"]["views"] == (c["before"] or {}).get("views", 0) + fs.K


def test_features_and_browse(demo):
    f = demo("/api/features")
    assert [x["name"] for x in f["features"]] == fs.FEATURES
    assert all(sum(x["hist"]) == f["sample"] for x in f["features"])
    b = demo("/api/browse", {"table": "fs_user_online", "where": "clicks >= 1", "order": "clicks DESC", "limit": 5})
    assert b["rows"] and b["rows"][0]["clicks"] >= b["rows"][-1]["clicks"]
    with pytest.raises(urllib.error.HTTPError):
        demo("/api/browse", {"table": "_strata_keys", "where": "", "order": ""})


# ------------------------------------------------------------------ workflows

import workflows as wfm  # noqa: E402


def test_workflow_checks_order_and_sql():
    specs = {s["name"]: wfm.check(s, {x["name"] for x in wfm.SEEDS}) for s in wfm.SEEDS}
    order = wfm.topo(specs)
    assert order.index("user_category") < order.index("user_category_share") > order.index("user_activity")
    assert order.index("item_popularity") < order.index("item_signals")
    # A windowed aggregate reads a window's own parameter, never :now - N.
    sql, _ = wfm.build_sql(specs["user_activity"], specs)
    assert ":since_300" in sql and ":now -" not in sql
    # A workflow input is read as of the run, from its history.
    sql, _ = wfm.build_sql(specs["user_category_share"], specs)
    assert "fv_user_category_hist" in sql and "fv_user_activity_hist" in sql and "<= :now" in sql
    cyc = dict(specs, user_activity=dict(specs["user_activity"], inputs=[{"workflow": "user_category_share", "alias": "a"}]))
    with pytest.raises(wfm.SpecError, match="cycle"):
        wfm.topo(cyc)
    for bad in ({"name": "X"}, dict(wfm.SEEDS[0], features=[{"name": "n", "fn": "median"}]),
                dict(wfm.SEEDS[3], features=[{"name": "n", "fn": "count"}]),
                dict(wfm.SEEDS[1], inputs=[wfm.SEEDS[1]["inputs"][0], {"table": "fs_items", "alias": "p"}])):
        with pytest.raises(wfm.SpecError):
            wfm.check(bad, set())


def test_workflows_chain_search_train_and_serve(demo):
    end = time.time() + 120
    while demo("/api/wf/graph")["status"] != "live" and time.time() < end:
        time.sleep(1)
    g = demo("/api/wf/graph")
    assert {n["name"] for n in g["nodes"] if n["kind"] == "workflow"} >= {s["name"] for s in wfm.SEEDS}
    # A workflow on two others: chained, joined, backfilled, served by key.
    spec = {"name": "t_chain", "inputs": [{"workflow": "user_category_share", "alias": "s"},
                                          {"workflow": "user_activity", "alias": "u", "on": ["s.entity", "entity"]}],
            "mode": "map", "key": "s.entity", "features": [{"name": "lift2", "expr": "s.lift * 2"}], "every": 10}
    pv = demo("/api/wf/preview", {"spec": spec})
    assert pv["rows"]
    demo("/api/wf/save", {"spec": spec, "backfill": 1})
    sp = demo("/api/wf/spec?name=user_category_share")
    assert "t_chain" in sp["downstream"]
    with pytest.raises(urllib.error.HTTPError):  # it feeds t_chain
        demo("/api/wf/delete", {"name": "user_category_share"})
    with pytest.raises(urllib.error.HTTPError):  # a cycle
        demo("/api/wf/save", {"spec": dict(spec, name="user_activity", inputs=[{"workflow": "t_chain", "alias": "s"}], key="s.entity")})
    # Search the statistics: the steps that match, what they feed, what feeds them.
    hits = demo("/api/wf/search", {"where": "feature = 'views' AND max > 0"})
    assert "user_activity" in hits["workflows"] and "t_chain" in hits["downstream"]
    hits = demo("/api/wf/search", {"where": "wf = 't_chain'"})
    assert hits["workflows"] == ["t_chain"] and "user_category" in hits["upstream"]
    with pytest.raises(urllib.error.HTTPError):
        demo("/api/wf/search", {"where": "1 = 1 UNION SELECT * FROM _strata_keys"})
    # A model on the workflows, trained as of each impression, then serving.
    m = demo("/api/wf/train", {"joins": [{"workflow": "user_activity", "on": "usr"}, {"workflow": "item_signals", "on": "item"},
                                         {"workflow": "user_category_share", "on": "uc"}],
                               "features": ["user_activity.ctr", "item_signals.hot", "user_category_share.affinity", "shop.price"]})
    assert m["train_rows"] > 500 and m["auc_true"] > 0.6
    demo("/api/deploy", {"id": m["id"]})
    r = demo("/api/shop?user=u010")
    assert r["model"] == m["id"] and len(r["items"]) == fs.K
    demo("/api/wf/delete", {"name": "t_chain"})
    # The deployed model reads item_signals, which no workflow reads: its
    # delete is refused while the model serves (DAP-11: it dropped the
    # table, and every ranking failed).
    with pytest.raises(urllib.error.HTTPError) as e:
        demo("/api/wf/delete", {"name": "item_signals"})
    assert e.value.code == 400 and "deployed model" in json.loads(e.value.read())["error"]
    assert demo("/api/shop?user=u011")["model"] == m["id"]
    assert "item_signals" in {n["name"] for n in demo("/api/wf/graph")["nodes"] if n["kind"] == "workflow"}
