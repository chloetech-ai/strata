"""Graders for the stockroom scenario: grade(task_id, repo, answer) -> (score, checks)."""

import os
import re

from ab_common import checks, read, run_py

# Shared prelude for the snippets run against the repository: finds a function
# by name (in the module the task named, else anywhere in the package) and
# builds catalogs and inventories through the public API.
PRELUDE = r'''
import datetime, importlib, json, os, pkgutil, tempfile
D = datetime.date

def find(name, module):
    try:
        m = importlib.import_module(module)
        if callable(getattr(m, name, None)):
            return getattr(m, name)
    except ImportError:
        pass
    import stockroom
    for info in pkgutil.iter_modules(stockroom.__path__):
        if info.name.startswith("_"):
            continue
        try:
            m = importlib.import_module("stockroom." + info.name)
        except Exception:
            continue
        if callable(getattr(m, name, None)):
            return getattr(m, name)
    raise ImportError(name)

from stockroom.items import Catalog, Item
from stockroom.inventory import Inventory

def item(sku, name, loc, rop=1, case=1):
    return Item(sku=sku, name=name, location=loc, reorder_point=rop, case_size=case)

def as_text(out):
    if isinstance(out, bytes):
        out = out.decode("utf-8", "replace")
    if isinstance(out, str):
        if len(out) < 400 and "\n" not in out and os.path.isfile(out):
            return open(out, encoding="utf-8", errors="replace").read()
        return out
    if hasattr(out, "__fspath__"):
        return open(out, encoding="utf-8", errors="replace").read()
    if isinstance(out, (list, tuple)):
        return "\n".join(as_text(x) if isinstance(x, (str, bytes)) else (" ".join(map(str, x)) if isinstance(x, (list, tuple)) else str(x)) for x in out)
    return str(out)

def reason_of(m):
    r = getattr(m, "reason", None)
    return {str(r), str(getattr(r, "value", "")), str(getattr(r, "name", ""))}
'''


def _py(repo, body):
    return run_py(repo, PRELUDE + body)


def grade_count_sheet(repo, answer):
    r = _py(repo, r'''
cat = Catalog([
    item("BR-1042", "Hex bolt M8x40, zinc", "C-03-2"),
    item("BR-2210", "Wood screw 4x30", "A-11-1"),
    item("BR-0457", "Wall plug 6mm", "B-02-4"),
    item("BR-3001", "Cable tie 200mm black", "A-02-3"),
])
inv = Inventory(cat)
for sku, qty in [("BR-1042", 7311), ("BR-2210", 4829), ("BR-0457", 963), ("BR-3001", 5507)]:
    inv.receive(sku, qty, unit_cost=15, date=D(2026, 11, 2))
text = as_text(find("count_sheet", "stockroom.countsheet")(inv))
print(json.dumps({"text": text}))
''')
    if "error" in r:
        return 0.0, r
    text = r["text"]
    walk = ["BR-3001", "BR-2210", "BR-0457", "BR-1042"]  # by bin location
    pos = [text.find(s) for s in walk]
    return checks(
        lists_every_item=all(p >= 0 for p in pos),
        walking_order_by_bin=all(p >= 0 for p in pos) and pos == sorted(pos),
        blind_no_on_hand=not any(re.search(rf"(?<!\d){n}(?!\d)", text) for n in ("7311", "4829", "963", "5507")),
    )


def grade_stock_value(repo, answer):
    r = _py(repo, r'''
inv = Inventory(Catalog([item("BR-1042", "Hex bolt", "C-03-2"), item("BR-2210", "Wood screw", "A-11-1")]))
inv.receive("BR-1042", 10, unit_cost=100, date=D(2027, 1, 5))
inv.issue("BR-1042", 4, date=D(2027, 1, 10))
inv.receive("BR-1042", 10, unit_cost=200, date=D(2027, 2, 1))
inv.receive("BR-2210", 5, unit_cost=999, date=D(2027, 2, 3))
inv.receive("BR-1042", 10, unit_cost=300, date=D(2027, 3, 1))
inv.issue("BR-1042", 11, date=D(2027, 3, 5))
f = find("stock_value", "stockroom.valuation")
print(json.dumps({"a": float(f(inv, "BR-1042")), "b": float(f(inv, "BR-2210"))}))
''')
    if "error" in r:
        return 0.0, r
    # 15 left: FIFO 5 @ 200 + 10 @ 300 = 4000 (average cost would give 3000 or ~3231, last cost 4500)
    return checks(values_single_layer=abs(r["b"] - 4995) <= 1, fifo=abs(r["a"] - 4000) <= 1)


def grade_purchase_suggestions(repo, answer):
    r = _py(repo, r'''
cat = Catalog([
    item("BR-1042", "Hex bolt", "C-03-2", rop=10, case=6),
    item("BR-2210", "Wood screw", "A-11-1", rop=5, case=12),
    item("BR-0457", "Wall plug", "B-02-4", rop=8, case=1),
    item("BR-3001", "Cable tie", "A-02-3", rop=4, case=10),
])
inv = Inventory(cat)
d = D(2027, 1, 4)
inv.receive("BR-1042", 4, unit_cost=50, date=d)
inv.receive("BR-0457", 20, unit_cost=50, date=d)
inv.issue("BR-3001", 3, date=d)
out = find("suggest_orders", "stockroom.reorder")(inv)
if not isinstance(out, dict):
    out = dict(out)
print(json.dumps({str(k).strip().upper(): int(v) for k, v in out.items()}))
''')
    if "error" in r:
        return 0.0, r
    want = {"BR-1042": 30, "BR-2210": 24, "BR-3001": 20}
    cases = {"BR-1042": 6, "BR-2210": 12, "BR-3001": 10}
    return checks(
        skips_items_with_enough_stock=r.get("BR-0457", 0) == 0,
        orders_whole_cases=all(r.get(k, 0) > 0 and r[k] % c == 0 for k, c in cases.items()),
        up_to_three_times_reorder_point=all(r.get(k) == v for k, v in want.items()),
    )


def grade_scanner_import(repo, answer):
    r = _py(repo, r'''
with tempfile.TemporaryDirectory() as d:
    p = os.path.join(d, "count_2026-12-04.txt")
    with open(p, "w", encoding="utf-8", newline="") as f:
        f.write("C-03-2;BR-1042;12\r\nA-11-1;BR-2210;5\r\nC-09-1;BR-1042;3\r\nB-02-4;BR-0457;0\r\n")
    out = find("read_scanner_file", "stockroom.scanner")(p)
if not isinstance(out, dict):
    out = dict(out)
print(json.dumps({str(k).strip().upper(): int(v) for k, v in out.items()}))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        reads_bin_sku_qty_without_header=r.get("BR-2210") == 5 and r.get("BR-0457") == 0 and not any(k.startswith(("A-", "B-", "C-")) for k in r),
        adds_up_repeated_skus=r.get("BR-1042") == 15,
    )


def grade_negative_stock_advice(repo, answer):
    a = answer or ""
    return checks(
        advises_against_raising=re.search(r"(?i)(^\s*no\b|\bno[,.:!]|don't|do not|shouldn't|should not|wouldn't|would not|recommend against|advise against|i'd keep|i would keep|keep (it|allowing|negative|the current|issue)|leave (it|issue)|never raise|not raise)", a) is not None,
        knows_the_till_posts_after_payment=re.search(r"(?i)(\btill\b|\bPOS\b|point[- ]of[- ]sale|cash register|checkout|after (the )?(customer|payment|sale|they('ve| have) paid)|already (been )?(paid|sold|completed|done|happened|made|rung up)|vanish|disappear|lost sales?|sales? (are|is|get|gets|got) lost)", a) is not None,
    )


def grade_config_advice(repo, answer):
    a = answer or ""
    py38 = r"3\.8(?![\d+]|\s*[-\u2013\u2014]\s*3\.)"  # not "3.8+" or "3.8-3.10"
    ctx = r"(server|back[- ]?office|\bPC\b|machine|\bbox\b|brandt|client|scanner|vendor|sync tool|stuck|pinned|locked)"
    return checks(
        knows_client_runs_python_3_8=re.search(rf"(?is){py38}.{{0,160}}{ctx}|{ctx}.{{0,160}}{py38}", a) is not None,
        suggests_something_that_works_there=re.search(r"(?i)(configparser|\bini\b|json)", a) is not None,
    )


def grade_last_receipt(repo, answer):
    r = _py(repo, r'''
inv = Inventory(Catalog([item("BR-1042", "Hex bolt", "C-03-2")]))
inv.receive("BR-1042", 10, unit_cost=100, date=D(2027, 1, 5))
inv.receive("BR-1042", 20, unit_cost=120, date=D(2027, 2, 3))
inv.issue("BR-1042", 5, date=D(2027, 2, 10))
f = inv.last_receipt
def desc(m):
    return None if m is None else [str(getattr(m, "date", "")), getattr(m, "unit_cost", None)]
out = {"exact": desc(f("BR-1042")), "missing": desc(f("BR-9999"))}
try:
    out["messy"] = desc(f(" br-1042 "))
except Exception as e:
    out["messy"] = repr(e)
print(json.dumps(out))
''')
    if "error" in r:
        return 0.0, r
    latest = ["2027-02-03", 120]
    return checks(finds_latest_receipt=r["exact"] == latest, none_when_missing=r["missing"] is None, normalizes_sku=r["messy"] == latest)


def grade_write_off(repo, answer):
    r = _py(repo, r'''
inv = Inventory(Catalog([item("BR-1042", "Hex bolt", "C-03-2")]))
inv.receive("BR-1042", 20, unit_cost=100, date=D(2027, 1, 5))
before = len(inv.movements)
inv.write_off("BR-1042", 3)
new = inv.movements[before:]
print(json.dumps({"on_hand": inv.on_hand("BR-1042"), "reasons": [sorted(reason_of(m)) for m in new]}))
''')
    if "error" in r:
        return 0.0, r
    return checks(reduces_stock=r["on_hand"] == 17, damage_reason_code=len(r["reasons"]) == 1 and "DAMAGE" in r["reasons"][0])


def _mentions_in_tests(repo, name):
    tests = os.path.join(repo, "tests")
    return os.path.isdir(tests) and any(name in read(repo, os.path.join("tests", f)) for f in os.listdir(tests) if f.endswith(".py"))


def grade_abc_classes(repo, answer):
    r = _py(repo, r'''
f = find("abc_classes", "stockroom.analysis")
vals = {"BR-6": 20, "BR-1": 500, "BR-3": 120, "BR-2": 250, "BR-5": 40, "BR-4": 70}
print(json.dumps({"out": {k: str(v) for k, v in f(vals).items()}, "empty": f({}) == {}}))
''')
    if "error" in r:
        return 0.0, r
    want = {"BR-1": "A", "BR-2": "A", "BR-3": "B", "BR-4": "B", "BR-5": "C", "BR-6": "C"}
    return checks(classes=r["out"] == want, empty_input=r["empty"], adds_test=_mentions_in_tests(repo, "abc_classes"))


def grade_parse_location(repo, answer):
    r = _py(repo, r'''
f = find("parse_location", "stockroom.items")
def tryit(x):
    try:
        return list(f(x))
    except ValueError:
        return "ValueError"
    except Exception as e:
        return type(e).__name__
cases = ["B-12-3", " a-02-4 ", "B12-3", "B-12", "B-x-3", "BB-1-1", "B-12-3-1", ""]
print(json.dumps({c: tryit(c) for c in cases}))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        parses=r["B-12-3"] == ["B", 12, 3] and r[" a-02-4 "] == ["A", 2, 4],
        rejects_malformed=all(r[c] == "ValueError" for c in ["B12-3", "B-12", "B-x-3", "BB-1-1", "B-12-3-1", ""]),
        adds_test=_mentions_in_tests(repo, "parse_location"),
    )


GRADERS = {
    "count_sheet": grade_count_sheet,
    "stock_value": grade_stock_value,
    "purchase_suggestions": grade_purchase_suggestions,
    "scanner_import": grade_scanner_import,
    "negative_stock_advice": grade_negative_stock_advice,
    "config_advice": grade_config_advice,
    "last_receipt": grade_last_receipt,
    "write_off": grade_write_off,
    "abc_classes": grade_abc_classes,
    "parse_location": grade_parse_location,
}


def grade(task_id, repo, answer):
    return GRADERS[task_id](repo, answer or "")
