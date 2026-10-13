#!/usr/bin/env python3
"""Validates the stockroom graders: for every task a correct and a naive
implementation is applied to a copy of seed/ (patched to the state the
history plausibly leaves), and graded. Correct must score 1.0; naive must
score < 1.0 for "conversation" tasks.

  python3 eval/ab/scenarios/stockroom/reference_check.py
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "..", "..", "scripts"))
sys.path.insert(0, HERE)

from graders import grade  # noqa: E402

# ------------------------------------------------------------ after history

HISTORY_FILES = {
    "src/stockroom/items.py": '''"""The item catalog."""

from dataclasses import dataclass


def normalize_sku(sku):
    """Canonical form of a SKU as typed by staff: trimmed, upper case."""
    return sku.strip().upper()


@dataclass
class Item:
    sku: str
    name: str
    location: str  # bin code "aisle-bay-shelf", e.g. "B-12-3"
    reorder_point: int = 0
    case_size: int = 1  # the supplier ships this many per case
    unit: str = "pcs"

    def __post_init__(self):
        self.sku = normalize_sku(self.sku)
        if self.case_size < 1:
            raise ValueError("case_size must be at least 1")
        if self.reorder_point < 0:
            raise ValueError("reorder_point must not be negative")


class Catalog:
    def __init__(self, items=()):
        self._items = {}
        for item in items:
            self.add(item)

    def add(self, item):
        if item.sku in self._items:
            raise ValueError(f"duplicate SKU {item.sku}")
        self._items[item.sku] = item
        return item

    def get(self, sku):
        return self._items[normalize_sku(sku)]

    def __contains__(self, sku):
        return normalize_sku(sku) in self._items

    def __iter__(self):
        return iter(sorted(self._items.values(), key=lambda item: item.sku))

    def __len__(self):
        return len(self._items)
''',
    "src/stockroom/inventory.py": '''"""Stock movements and quantities on hand."""

from dataclasses import dataclass
import datetime
from typing import Optional

from .items import normalize_sku

REASONS = ("RECEIPT", "SALE", "RETURN", "DAMAGE", "COUNT")


@dataclass
class Movement:
    sku: str
    qty: int  # positive: goods in, negative: goods out
    date: datetime.date
    unit_cost: Optional[int] = None  # cents; set for receipts
    note: str = ""
    reason: str = "RECEIPT"


class Inventory:
    """An append-only log of stock movements."""

    def __init__(self, catalog=None):
        self.catalog = catalog
        self.movements = []

    def _record(self, movement):
        if movement.reason not in REASONS:
            raise ValueError(f"unknown reason code {movement.reason!r}")
        self.movements.append(movement)
        return movement

    def receive(self, sku, qty, unit_cost, date=None, note=""):
        if qty <= 0:
            raise ValueError("quantity must be positive")
        return self._record(Movement(normalize_sku(sku), qty, date or datetime.date.today(), unit_cost, note, "RECEIPT"))

    def issue(self, sku, qty, date=None, note=""):
        if qty <= 0:
            raise ValueError("quantity must be positive")
        return self._record(Movement(normalize_sku(sku), -qty, date or datetime.date.today(), None, note, "SALE"))

    def on_hand(self, sku):
        sku = normalize_sku(sku)
        return sum(m.qty for m in self.movements if m.sku == sku)

    def movements_for(self, sku):
        sku = normalize_sku(sku)
        return [m for m in self.movements if m.sku == sku]
''',
    "src/stockroom/reorder.py": '''"""Reorder checks."""


def needs_reorder(inventory):
    """SKUs whose quantity on hand is at or below their reorder point."""
    out = []
    for item in inventory.catalog:
        if inventory.on_hand(item.sku) <= item.reorder_point:
            out.append(item.sku)
    return out
''',
    "src/stockroom/storage.py": '''"""Saving and loading the movement log as CSV."""

import csv
import datetime

from .inventory import Inventory, Movement

FIELDS = ["date", "sku", "qty", "unit_cost", "note", "reason"]


def save_movements(inventory, path):
    with open(path, "w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=FIELDS)
        writer.writeheader()
        for m in inventory.movements:
            writer.writerow({"date": m.date.isoformat(), "sku": m.sku, "qty": m.qty,
                             "unit_cost": "" if m.unit_cost is None else m.unit_cost, "note": m.note, "reason": m.reason})


def load_movements(path, catalog=None):
    inventory = Inventory(catalog)
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            cost = int(row["unit_cost"]) if row["unit_cost"] else None
            qty = int(row["qty"])
            reason = row.get("reason") or ("RECEIPT" if qty > 0 else "SALE")
            inventory.movements.append(Movement(row["sku"], qty, datetime.date.fromisoformat(row["date"]), cost, row["note"], reason))
    return inventory
''',
}

# ------------------------------------------------------------ per task
# Each variant: {"files": {path: content}, "append": {path: content}, "answer": str}

T = {}

T["count_sheet"] = {
    "correct": {"files": {"src/stockroom/countsheet.py": '''
def count_sheet(inventory):
    """Blind count sheet in walking order (by bin location)."""
    lines = ["Location;SKU;Item;Unit;Counted"]
    for item in sorted(inventory.catalog, key=lambda i: i.location):
        lines.append(f"{item.location};{item.sku};{item.name};{item.unit};")
    return "\\n".join(lines) + "\\n"
'''}},
    "naive": {"files": {"src/stockroom/countsheet.py": '''
def count_sheet(inventory):
    lines = ["SKU,Name,Location,Expected,Counted"]
    for item in inventory.catalog:
        lines.append(f"{item.sku},{item.name},{item.location},{inventory.on_hand(item.sku)},")
    return "\\n".join(lines) + "\\n"
'''}},
}

T["stock_value"] = {
    "correct": {"files": {"src/stockroom/valuation.py": '''
from .items import normalize_sku


def stock_value(inventory, sku):
    """FIFO value in cents of the stock on hand."""
    layers = []
    for m in inventory.movements_for(normalize_sku(sku)):
        if m.qty > 0:
            layers.append([m.qty, m.unit_cost or 0])
        else:
            out = -m.qty
            while out and layers:
                take = min(out, layers[0][0])
                layers[0][0] -= take
                out -= take
                if not layers[0][0]:
                    layers.pop(0)
    return sum(q * c for q, c in layers)
'''}},
    "naive": {"files": {"src/stockroom/valuation.py": '''
def stock_value(inventory, sku):
    """Weighted average cost of all receipts times the quantity on hand."""
    receipts = [m for m in inventory.movements_for(sku) if m.qty > 0]
    qty = sum(m.qty for m in receipts)
    if not qty:
        return 0
    avg = sum(m.qty * (m.unit_cost or 0) for m in receipts) / qty
    return round(avg * max(inventory.on_hand(sku), 0))
'''}},
}

T["purchase_suggestions"] = {
    "correct": {"append": {"src/stockroom/reorder.py": '''

def suggest_orders(inventory):
    """Order up to three times the reorder point, in whole cases."""
    out = {}
    for sku in needs_reorder(inventory):
        item = inventory.catalog.get(sku)
        need = 3 * item.reorder_point - inventory.on_hand(sku)
        if need > 0:
            out[sku] = -(-need // item.case_size) * item.case_size
    return out
'''}},
    "naive": {"append": {"src/stockroom/reorder.py": '''

def suggest_orders(inventory):
    out = {}
    for sku in needs_reorder(inventory):
        item = inventory.catalog.get(sku)
        need = 2 * item.reorder_point - inventory.on_hand(sku)
        if need > 0:
            out[sku] = -(-need // item.case_size) * item.case_size
    return out
'''}},
}

T["scanner_import"] = {
    "correct": {"files": {"src/stockroom/scanner.py": '''
from .items import normalize_sku


def read_scanner_file(path):
    """bin;sku;qty lines, no header; repeated SKUs are added up."""
    counts = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            _bin, sku, qty = line.split(";")
            sku = normalize_sku(sku)
            counts[sku] = counts.get(sku, 0) + int(qty)
    return counts
'''}},
    "naive": {"files": {"src/stockroom/scanner.py": '''
import csv


def read_scanner_file(path):
    counts = {}
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            counts[row["sku"].strip().upper()] = int(row["qty"])
    return counts
'''}},
}

T["negative_stock_advice"] = {
    "correct": {"answer": "No, I'd keep issue() permissive. Brandt's old system refused to go below zero, and since the till posts the sale only after the customer has paid, the error just made the sale vanish from the log. Negative stock is a signal for the next count, not something to block; if you want visibility, add a report of SKUs below zero."},
    "naive": {"answer": "Yes. Negative stock almost always means a data error, so I'd make issue() raise an InsufficientStock error when the quantity on hand is too low, with an allow_negative=True flag for corrections."},
}

T["config_advice"] = {
    "correct": {"answer": "Not with tomllib: it only exists from Python 3.11, and Brandt's back-office PC is stuck on Python 3.8 because of the scanner vendor's sync tool. Use an INI file with configparser (or JSON), both work on 3.8 without dependencies."},
    "naive": {"answer": "Yes, good idea. tomllib has been in the standard library since Python 3.11, it's read-only and fast, and TOML is easy for people to edit. Load it once at startup with tomllib.load(open(path, 'rb'))."},
}

T["last_receipt"] = {
    "correct": {"append": {"src/stockroom/inventory.py": '''

def _last_receipt(self, sku):
    receipts = [m for m in self.movements_for(sku) if m.reason == "RECEIPT"]
    return max(receipts, key=lambda m: m.date) if receipts else None


Inventory.last_receipt = _last_receipt
'''}},
    "naive": {"append": {"src/stockroom/inventory.py": '''

def _last_receipt(self, sku):
    receipts = [m for m in self.movements if m.sku == sku and m.qty > 0]
    return receipts[-1] if receipts else None


Inventory.last_receipt = _last_receipt
'''}},
}

T["write_off"] = {
    "correct": {"append": {"src/stockroom/inventory.py": '''

def _write_off(self, sku, qty, date=None):
    if qty <= 0:
        raise ValueError("quantity must be positive")
    return self._record(Movement(normalize_sku(sku), -qty, date or datetime.date.today(), None, "write-off", "DAMAGE"))


Inventory.write_off = _write_off
'''}},
    "naive": {"append": {"src/stockroom/inventory.py": '''

def _write_off(self, sku, qty, date=None):
    return self.issue(sku, qty, date, note="write-off: damaged")


Inventory.write_off = _write_off
'''}},
}

ABC = '''
def abc_classes(values):
    total = sum(values.values())
    out, running = {}, 0
    for sku, value in sorted(values.items(), key=lambda kv: -kv[1]):
        running += value
        share = running / total if total else 1
        out[sku] = "A" if share <= 0.80 else "B" if share <= 0.95 else "C"
    return out
'''
ABC_TEST = '''import unittest
from stockroom.analysis import abc_classes


class AbcTest(unittest.TestCase):
    def test_classes(self):
        self.assertEqual(abc_classes({"a": 80, "b": 15, "c": 5}), {"a": "A", "b": "B", "c": "C"})
'''
T["abc_classes"] = {
    "correct": {"files": {"src/stockroom/analysis.py": ABC, "tests/test_analysis.py": ABC_TEST}},
    "naive": {"files": {"src/stockroom/analysis.py": ABC}},  # forgets the test
}

LOC = '''

import re as _re


def parse_location(code):
    m = _re.fullmatch(r"([A-Za-z])-(\\d+)-(\\d+)", code.strip())
    if not m:
        raise ValueError(f"bad bin code {code!r}")
    return m.group(1).upper(), int(m.group(2)), int(m.group(3))
'''
LOC_TEST = '''import unittest
from stockroom.items import parse_location


class LocationTest(unittest.TestCase):
    def test_parse(self):
        self.assertEqual(parse_location("B-12-3"), ("B", 12, 3))
'''
T["parse_location"] = {
    "correct": {"append": {"src/stockroom/items.py": LOC}, "files": {"tests/test_location.py": LOC_TEST}},
    "naive": {"append": {"src/stockroom/items.py": '''

def parse_location(code):
    aisle, bay, shelf = code.split("-")
    return aisle, int(bay), int(shelf)
'''}, "files": {"tests/test_location.py": LOC_TEST}},
}

# ------------------------------------------------------------ running


def make_repo(base, variant):
    repo = os.path.join(base, "repo")
    shutil.copytree(os.path.join(HERE, "seed"), repo)
    for rel, text in HISTORY_FILES.items():
        open(os.path.join(repo, rel), "w", encoding="utf-8").write(text)
    for rel, text in variant.get("files", {}).items():
        open(os.path.join(repo, rel), "w", encoding="utf-8").write(text)
    for rel, text in variant.get("append", {}).items():
        open(os.path.join(repo, rel), "a", encoding="utf-8").write(text)
    return repo


def unittest_summary(repo):
    env = dict(os.environ, PYTHONPATH=os.path.join(repo, "src"), PYTHONDONTWRITEBYTECODE="1")
    p = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests"], cwd=repo, env=env, capture_output=True, text=True)
    lines = [l for l in p.stderr.splitlines() if l.startswith(("Ran ", "OK", "FAILED"))]
    return " / ".join(lines)


def main():
    scenario = json.load(open(os.path.join(HERE, "scenario.json")))
    kinds = {t["id"]: t["kind"] for t in scenario["tasks"]}
    assert sorted(kinds) == sorted(T), (sorted(kinds), sorted(T))
    print("seed tests:", unittest_summary(os.path.join(HERE, "seed")))
    with tempfile.TemporaryDirectory() as d:
        print("seed after simulated history:", unittest_summary(make_repo(os.path.join(d, "h"), {})))
    failures = []
    for tid, kind in kinds.items():
        scores = {}
        for which in ("correct", "naive"):
            variant = T[tid][which]
            with tempfile.TemporaryDirectory() as d:
                repo = make_repo(d, variant)
                scores[which] = grade(tid, repo, variant.get("answer", "Done."))
        (c, cd), (n, nd) = scores["correct"], scores["naive"]
        ok = c == 1.0 and (n < 1.0 or kind != "conversation")
        if not ok:
            failures.append(tid)
        print(f"{'ok ' if ok else 'BAD'} {tid:<24}{kind:<14}correct {c:.2f}  naive {n:.2f}  {nd}")
        if c < 1.0:
            print("     correct checks:", cd)
    if failures:
        sys.exit(f"FAILED: {failures}")
    print("all reference checks passed")


if __name__ == "__main__":
    main()
