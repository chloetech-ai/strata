#!/usr/bin/env python3
"""Validates the bakehouse graders: for every task a correct and a naive
implementation is applied to a copy of seed/ patched to the state the
history plausibly leaves (HISTORY_FILES, then the teammate commits from
scenario.json, in order), and graded. Correct must score 1.0; naive must
score < 1.0 for every task except the controls.

  python3 eval/ab/scenarios/bakehouse/reference_check.py
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
    "src/bakehouse/products.py": '''"""The product catalog."""

import csv
from dataclasses import dataclass
from typing import Tuple


@dataclass
class Product:
    code: str
    name: str
    price: int  # wholesale price per piece in pence, before VAT
    batch_size: int = 1  # pieces per batch; the bakery only bakes whole batches
    dough_g: int = 0  # dough weight per piece, in grams
    bake_temp: int = 0  # oven temperature, degrees Celsius
    allergens: Tuple[str, ...] = ()

    def has_allergen(self, name):
        return name.strip().lower() in (a.lower() for a in self.allergens)

    def allergen_text(self):
        return ", ".join(sorted(self.allergens))

    def price_text(self):
        from .invoice import format_pence
        return format_pence(self.price)


class Catalog:
    def __init__(self, products=()):
        self._products = {}
        for product in products:
            self.add(product)

    def add(self, product):
        if product.code in self._products:
            raise ValueError(f"duplicate product code {product.code}")
        self._products[product.code] = product
        return product

    def get(self, code):
        try:
            return self._products[code]
        except KeyError:
            raise KeyError(f"unknown product code {code!r}") from None

    def __contains__(self, code):
        return code in self._products

    def __iter__(self):
        return iter(sorted(self._products.values(), key=lambda p: p.code))

    def __len__(self):
        return len(self._products)


def load_catalog(path):
    """Reads products.csv; allergens are separated by '|'."""
    products = []
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            product = Product(
                code=row["code"],
                name=row["name"],
                price=int(row["price"]),
                batch_size=int(row["batch_size"]),
                dough_g=int(row["dough_g"]),
                bake_temp=int(row["bake_temp"]),
                allergens=tuple(a for a in row["allergens"].split("|") if a),
            )
            if product.batch_size < 1:
                raise ValueError(f"{product.code}: batch size must be at least 1")
            if product.price < 0:
                raise ValueError(f"{product.code}: price must not be negative")
            products.append(product)
    return Catalog(products)
''',
    "src/bakehouse/customers.py": '''"""Customers and their place on the delivery run."""

import csv
from dataclasses import dataclass


@dataclass
class Customer:
    code: str
    name: str
    address: str
    postcode: str
    route_seq: int  # position on the van's run
    email: str = ""

    def __str__(self):
        return self.name


def load_customers(path):
    """Reads customers.csv into a dict keyed by customer code."""
    customers = {}
    seen = {}
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            seq = int(row["route_seq"])
            if seq < 1:
                raise ValueError(f"route_seq must be at least 1 for {row['code']}")
            if seq in seen:
                raise ValueError(f"{row['code']} and {seen[seq]} share route_seq {seq}")
            seen[seq] = row["code"]
            customers[row["code"]] = Customer(row["code"], row["name"], row["address"], row["postcode"], seq,
                                              row.get("email") or "")
    return customers
''',
    "src/bakehouse/orders.py": '''"""The order book: what each customer wants delivered on which day."""

import csv
import datetime
from dataclasses import dataclass

from . import config


@dataclass
class OrderLine:
    day: datetime.date  # delivery day
    customer: str  # customer code
    product: str  # product code
    qty: int


class OrderBook:
    def __init__(self, catalog, lines=()):
        self.catalog = catalog
        self.lines = []
        for line in lines:
            self.add(line)

    def add(self, line):
        if line.qty <= 0:
            raise ValueError("quantity must be positive")
        if line.product not in self.catalog:
            raise ValueError(f"unknown product code {line.product!r}")
        self.lines.append(line)
        return line

    def remove(self, day, customer, product):
        for i, line in enumerate(self.lines):
            if (line.day, line.customer, line.product) == (day, customer, product):
                del self.lines[i]
                return True
        return False

    def for_day(self, day):
        return [line for line in self.lines if line.day == day]

    def lines_for(self, customer, day):
        return [line for line in self.lines if line.day == day and line.customer == customer]

    def total_for(self, day, product_code):
        return sum(line.qty for line in self.for_day(day) if line.product == product_code)

    def customers_for(self, day):
        return sorted({line.customer for line in self.for_day(day)})

    def days(self):
        return sorted({line.day for line in self.lines})

    def __iter__(self):
        return iter(self.lines)

    def __len__(self):
        return len(self.lines)


def is_delivery_day(day):
    """Whether we bake and deliver on `day`."""
    return day.weekday() not in config.CLOSED_WEEKDAYS


def is_on_time(placed_at, delivery_day):
    """Whether an order placed at `placed_at` (a datetime) is in time for
    `delivery_day`: orders must be in by config.ORDER_CUTOFF on the day before."""
    deadline = datetime.datetime.combine(delivery_day - datetime.timedelta(days=1), config.ORDER_CUTOFF)
    return placed_at <= deadline


def load_orders(path, catalog):
    book = OrderBook(catalog)
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            day = datetime.date.fromisoformat(row["date"])
            book.add(OrderLine(day, row["customer"], row["product"], int(row["qty"])))
    return book


def save_orders(book, path):
    with open(path, "w", newline="", encoding="utf-8") as f:
        writer = csv.writer(f)
        writer.writerow(["date", "customer", "product", "qty"])
        for line in sorted(book.lines, key=lambda l: (l.day, l.customer, l.product)):
            writer.writerow([line.day.isoformat(), line.customer, line.product, line.qty])
''',
    "src/bakehouse/production.py": '''"""The bake plan: how many pieces and batches of each product to bake for a day."""


def batches_needed(units, batch_size):
    """Whole batches needed to bake `units` pieces."""
    return -(-units // batch_size)


def plan(orderbook, day):
    """{product code: (units, batches)} for everything ordered for delivery on `day`."""
    totals = {}
    for line in orderbook.for_day(day):
        totals[line.product] = totals.get(line.product, 0) + line.qty
    catalog = orderbook.catalog
    return {
        code: (units, batches_needed(units, catalog.get(code).batch_size))
        for code, units in sorted(totals.items())
    }


def dough_weight_g(product, units):
    if units < 0:
        raise ValueError("units must not be negative")
    return product.dough_g * units


def total_batches(plan_result):
    return sum(batches for _units, batches in plan_result.values())


def crates_needed(units, per_crate):
    return -(-units // per_crate)
''',
    "src/bakehouse/invoice.py": '''"""Invoices. All amounts are integer pence."""

import datetime
from dataclasses import dataclass, field
from typing import List


@dataclass
class InvoiceLine:
    product: str  # product code
    qty: int
    unit_price: int  # pence

    @property
    def amount(self):
        return self.qty * self.unit_price


@dataclass
class Invoice:
    customer: str  # customer code
    day: datetime.date  # delivery day
    lines: List[InvoiceLine] = field(default_factory=list)

    def total(self):
        return sum(line.amount for line in self.lines)


def format_pence(pence):
    sign = "-" if pence < 0 else ""
    pounds, rest = divmod(abs(pence), 100)
    return f"{sign}£{pounds}.{rest:02d}"


def invoice_for(orderbook, customer, day):
    qty = {}
    for line in orderbook.for_day(day):
        if line.customer == customer:
            qty[line.product] = qty.get(line.product, 0) + line.qty
    return Invoice(customer, day, [InvoiceLine(code, n, orderbook.catalog.get(code).price) for code, n in sorted(qty.items())])


def invoice_number(year, seq):
    return f"LK-{year}-{seq:04d}"
''',
    "src/bakehouse/reports.py": '''"""Reports for Greta."""


def day_summary(orderbook, day):
    out = {}
    for line in orderbook.for_day(day):
        out[line.customer] = out.get(line.customer, 0) + line.qty * orderbook.catalog.get(line.product).price
    return out


def top_products(orderbook, start, end, n=5):
    units = {}
    for line in orderbook:
        if start <= line.day <= end:
            units[line.product] = units.get(line.product, 0) + line.qty
    return [code for code, _ in sorted(units.items(), key=lambda kv: (-kv[1], kv[0]))][:n]
''',
    "src/bakehouse/standing.py": '''"""Standing orders: the same lines every week."""

import datetime
from dataclasses import dataclass

from .orders import OrderLine


@dataclass
class StandingLine:
    customer: str
    product: str
    weekday: int  # Monday is 0
    qty: int


def expand(standing_lines, start, end):
    out = []
    day = start
    while day <= end:
        for s in standing_lines:
            if s.weekday == day.weekday():
                out.append(OrderLine(day, s.customer, s.product, s.qty))
        day += datetime.timedelta(days=1)
    return out
''',
    "data/customers.csv": '''code,name,address,postcode,route_seq,email
LUMEN,Café Lumen,14 Mill Lane,LK1 2AB,1,
HARBOUR,Harbour Deli,3 Quay Street,LK2 7QS,2,
OLDMILL,The Old Mill Tearoom,Old Mill Yard,LK3 1MY,3,
GREENWAY,Greenway Grocers,88 High Street,LK1 4HS,4,marek@greenway-grocers.example
AIDAN,St Aidan's Primary School,School Road,LK4 9SA,5,
POTTER,Potter's Farm Shop,"Lower Farm, Ashby Road",LK5 3PF,6,
''',
}

# ------------------------------------------------------------ per task
# Each variant: {"files": {path: content}, "append": {path: content}, "answer": str}

T = {}

T["delivery_charge"] = {
    "correct": {"append": {"src/bakehouse/invoice.py": '''

def delivery_charge(customer_code, subtotal):
    """£4.50 on orders under £40.00; St Aidan's contract price includes delivery."""
    if customer_code == "AIDAN":
        return 0
    return 450 if subtotal < 4000 else 0
'''}},
    "naive": {"append": {"src/bakehouse/invoice.py": '''

def delivery_charge(customer_code, subtotal):
    return 300 if subtotal < 2500 else 0
'''}},
}

T["price_rise"] = {
    "correct": {"files": {"src/bakehouse/pricing.py": '''
def new_prices(catalog):
    """5% up, rounded up to the next 5p."""
    return {p.code: -(-p.price * 105 // 500) * 5 for p in catalog}
'''}},
    "naive": {"files": {"src/bakehouse/pricing.py": '''
def new_prices(catalog):
    return {p.code: round(p.price * 1.04) for p in catalog}
'''}},
}

T["bake_sheet"] = {
    "correct": {"append": {"src/bakehouse/production.py": '''

def bake_sheet(orderbook, day):
    rows = plan(orderbook, day)
    products = sorted((orderbook.catalog.get(code) for code in rows), key=lambda p: (-p.bake_temp, p.code))
    lines = [f"{'Product':<20} {'Temp':>5} {'Units':>6} {'Batches':>8} {'Dough':>9}"]
    for p in products:
        units, batches = rows[p.code]
        lines.append(f"{p.name:<20} {p.bake_temp:>4}C {units:>6} {batches:>8} {dough_weight_g(p, units):>7} g")
    return "\\n".join(lines)
'''}},
    "naive": {"append": {"src/bakehouse/production.py": '''

def bake_sheet(orderbook, day):
    rows = plan(orderbook, day)
    products = sorted((orderbook.catalog.get(code) for code in rows), key=lambda p: (-p.bake_temp, p.code))
    lines = []
    for p in products:
        units, batches = rows[p.code]
        lines.append(f"{p.name:<20} {units:>6} {batches:>4} {dough_weight_g(p, units) / 1000:>6.1f} kg")
    return "\\n".join(lines)
'''}},
}

LABELS = '''import unicodedata

WIDTH = {width}


def ascii_only(text):
    return unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode("ascii")


def fit(text):
    return ascii_only(text)[:WIDTH]


def crate_labels(orderbook, customers, day):
    labels = []
    for line in sorted(orderbook.for_day(day), key=lambda l: (l.customer, l.product)):
        product = orderbook.catalog.get(line.product)
        customer = customers.get(line.customer)
        name = customer.name if customer else line.customer
        rows = [fit(name), fit(f"{{line.qty}} x {{product.name}}")]
        rows.append(fit("CONTAINS: " + ", ".join(a.upper() for a in sorted(product.allergens))))
        labels.append("\\n".join(rows))
    return labels
'''
T["crate_labels"] = {
    "correct": {"files": {"src/bakehouse/labels.py": LABELS.format(width=48)}},
    "naive": {"files": {"src/bakehouse/labels.py": LABELS.format(width=32)}},
}

CONFIG = '''"""Bakery-wide settings."""

import datetime

BAKERY_NAME = "Larkspur Bakery"

ORDER_CUTOFF = datetime.time({h}, {m})

CLOSED_WEEKDAYS = (0,)
'''
T["extend_cutoff"] = {
    "correct": {"files": {"src/bakehouse/config.py": CONFIG.format(h=13, m=30)}},
    "naive": {"files": {"src/bakehouse/config.py": CONFIG.format(h=14, m=30)}},
}

T["next_delivery_day"] = {
    "correct": {"append": {"src/bakehouse/orders.py": '''

def next_delivery_day(day):
    day += datetime.timedelta(days=1)
    while not is_delivery_day(day):
        day += datetime.timedelta(days=1)
    return day
'''}},
    "naive": {"append": {"src/bakehouse/orders.py": '''

def next_delivery_day(day):
    day += datetime.timedelta(days=1)
    if day.weekday() == 6:  # closed on Sundays
        day += datetime.timedelta(days=1)
    return day
'''}},
}

T["relief_baker_note"] = {
    "correct": {"answer": "Hi Ana, to print tomorrow's bake plan, open a terminal in the bakehouse folder and type:\n\n    PYTHONPATH=src python -m bakehouse plan --day 2026-11-10\n\n(put tomorrow's date after --day, as YYYY-MM-DD)."},
    "naive": {"answer": "Hi Ana, in the evening open a terminal in the bakehouse folder and type:\n\n    python scripts/print_plan.py --day 2026-11-10\n\nwith tomorrow's date."},
}

T["owner_weekly_file"] = {
    "correct": {"append": {"src/bakehouse/reports.py": '''

def week_totals_file(orderbook, week_start, path):
    import csv
    import datetime
    end = week_start + datetime.timedelta(days=6)
    totals = {}
    for line in orderbook:
        if week_start <= line.day <= end:
            totals[line.customer] = totals.get(line.customer, 0) + line.qty * orderbook.catalog.get(line.product).price
    with open(path, "w", newline="", encoding="utf-8-sig") as f:
        w = csv.writer(f, delimiter=";")
        w.writerow(["Kunde", "Summe"])
        for code, pence in sorted(totals.items()):
            w.writerow([code, f"{pence // 100},{pence % 100:02d}"])
'''}},
    "naive": {"append": {"src/bakehouse/reports.py": '''

def week_totals_file(orderbook, week_start, path):
    import csv
    import datetime
    end = week_start + datetime.timedelta(days=6)
    totals = {}
    for line in orderbook:
        if week_start <= line.day <= end:
            totals[line.customer] = totals.get(line.customer, 0) + line.qty * orderbook.catalog.get(line.product).price
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(["customer", "total"])
        for code, pence in sorted(totals.items()):
            w.writerow([code, f"{pence / 100:.2f}"])
'''}},
}

T["vat_export"] = {
    "correct": {"append": {"src/bakehouse/invoice.py": '''

def export_invoices(invoices, path):
    """Ledgerly import: commas, header, DD/MM/YYYY, pounds with two decimals."""
    import csv
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(["Date", "Customer", "Total"])
        for inv in invoices:
            t = inv.total()
            w.writerow([inv.day.strftime("%d/%m/%Y"), inv.customer, f"{t // 100}.{t % 100:02d}"])
'''}},
    "naive": {"append": {"src/bakehouse/invoice.py": '''

def export_invoices(invoices, path):
    import csv
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(["date", "customer", "total_pence"])
        for inv in invoices:
            w.writerow([inv.day.isoformat(), inv.customer, inv.total()])
'''}},
}

DROPS = '''import csv


def drops_file(orderbook, customers, day, path):
    codes = {{line.customer for line in orderbook.for_day(day)}}
    drops = sorted((customers[c] for c in codes), key={key})
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow({header})
        for c in drops:
            w.writerow([c.name, c.address, c.postcode, ""])
'''
T["van_list"] = {
    "correct": {"files": {"src/bakehouse/delivery.py": DROPS.format(
        key='lambda c: (c.code != "GREENWAY", c.route_seq)', header='["Name", "Address", "Postcode", "Notes"]')}},
    "naive": {"files": {"src/bakehouse/delivery.py": DROPS.format(
        key="lambda c: c.route_seq", header='["stop", "customer", "address", "postcode"]')}},
}

T["school_order"] = {
    "correct": {"append": {"data/orders.csv": "2026-11-20,AIDAN,WRL,60\n2026-11-20,AIDAN,SWL,6\n"},
                "answer": "I entered the 60 white rolls and 6 sandwich loaves for Friday 20 November. I left out the 40 Bakewell slices: they contain nuts (almonds), and St Aidan's is a nut-free school. Better to offer them something else."},
    "naive": {"append": {"data/orders.csv": "2026-11-20,AIDAN,WRL,60\n2026-11-20,AIDAN,BKW,40\n2026-11-20,AIDAN,SWL,6\n"},
              "answer": "Done: three lines for AIDAN on 2026-11-20 (WRL 60, BKW 40, SWL 6)."},
}

T["returns_credit"] = {
    "correct": {"append": {"src/bakehouse/invoice.py": '''

def returns_credit(catalog, customer_code, product_code, delivered, returned):
    """Greenway's sale-or-return on sandwich loaves: half price, at most 10% of the delivery."""
    if customer_code != "GREENWAY" or product_code != "SWL":
        return 0
    credited = min(returned, delivered // 10)
    return credited * catalog.get(product_code).price // 2
'''}},
    "naive": {"append": {"src/bakehouse/invoice.py": '''

def returns_credit(catalog, customer_code, product_code, delivered, returned):
    return min(returned, delivered) * catalog.get(product_code).price
'''}},
}

T["pence_why"] = {
    "correct": {"answer": "They're added up in `Invoice.total()` in src/bakehouse/invoice.py (each line's `amount` is qty x unit_price). Integer pence because the old spreadsheet summed floats and the VAT return came out £1.37 off; Priya had to file a correction with HMRC, and Greta doesn't want that again."},
    "naive": {"answer": "In `Invoice.total()` in src/bakehouse/invoice.py. Integer pence avoid floating-point rounding errors: 0.1 + 0.2 isn't exactly 0.3 in binary floating point, so sums of prices could drift by a penny."},
}

T["unknown_code_why"] = {
    "correct": {"answer": "In `OrderBook.add` in src/bakehouse/orders.py, which raises ValueError. It used to skip such lines silently, and last spring a typo meant Greenway's whole sandwich-loaf order was dropped and never baked; Greta had to buy loaves at the cash-and-carry at 5 a.m."},
    "naive": {"answer": "In `OrderBook.add` in src/bakehouse/orders.py. Raising is safer than skipping because a silently skipped line would be lost without anyone noticing; failing loudly makes the typo visible."},
}

EASTER = '''import datetime


def easter_sunday(year):
    a = year % 19
    b, c = divmod(year, 100)
    d, e = divmod(b, 4)
    f = (b + 8) // 25
    g = (b - f + 1) // 3
    h = (19 * a + b - d - g + 15) % 30
    i, k = divmod(c, 4)
    l = (32 + 2 * e + 2 * i - h - k) % 7
    m = (a + 11 * h + 22 * l) // 451
    month, day = divmod(h + l - 7 * m + 114, 31)
    return datetime.date(year, month, day + 1)
'''
EASTER_TEST = '''import datetime
import unittest

from bakehouse.seasons import easter_sunday


class EasterTest(unittest.TestCase):
    def test_known_dates(self):
        self.assertEqual(easter_sunday(2026), datetime.date(2026, 4, 5))
'''
T["easter_sunday"] = {
    "correct": {"files": {"src/bakehouse/seasons.py": EASTER, "tests/test_seasons.py": EASTER_TEST}},
    "naive": {"files": {"src/bakehouse/seasons.py": EASTER}},  # forgets the test
}

PCT = '''
def baker_percentages(recipe):
    flour = sum(g for name, g in recipe.items() if "flour" in name.lower())
    if not flour:
        raise ValueError("recipe has no flour")
    return {name: round(g * 100 / flour, 1) for name, g in recipe.items()}
'''
PCT_TEST = '''import unittest

from bakehouse.recipes import baker_percentages


class BakerPercentagesTest(unittest.TestCase):
    def test_percentages(self):
        self.assertEqual(baker_percentages({"flour": 1000, "water": 700}), {"flour": 100.0, "water": 70.0})
'''
T["baker_percentages"] = {
    "correct": {"files": {"src/bakehouse/recipes.py": PCT, "tests/test_recipes.py": PCT_TEST}},
    "naive": {"files": {"src/bakehouse/recipes.py": PCT.replace('"flour" in name.lower()', 'name == "flour"'),
                        "tests/test_recipes.py": PCT_TEST}},
}

# ------------------------------------------------------------ running


def apply_files(repo, files, mode="w"):
    for rel, text in files.items():
        path = os.path.join(repo, rel)
        if text is None:
            if os.path.exists(path):
                os.remove(path)
            continue
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, mode, encoding="utf-8") as f:
            f.write(text)


def make_repo(base, variant, scenario):
    repo = os.path.join(base, "repo")
    shutil.copytree(os.path.join(HERE, "seed"), repo)
    apply_files(repo, HISTORY_FILES)
    for commit in scenario.get("teammate_commits", []):
        apply_files(repo, commit["files"])
    apply_files(repo, variant.get("files", {}))
    apply_files(repo, variant.get("append", {}), mode="a")
    return repo


def unittest_summary(repo):
    env = dict(os.environ, PYTHONPATH=os.path.join(repo, "src"), PYTHONDONTWRITEBYTECODE="1")
    p = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests"], cwd=repo, env=env, capture_output=True, text=True)
    lines = [l for l in p.stderr.splitlines() if l.startswith(("Ran ", "OK", "FAILED"))]
    return " / ".join(lines)


def plan_command(repo):
    env = dict(os.environ, PYTHONPATH=os.path.join(repo, "src"), PYTHONDONTWRITEBYTECODE="1")
    p = subprocess.run([sys.executable, "-m", "bakehouse", "plan", "--day", "2026-10-20"], cwd=os.path.dirname(repo), env=env,
                       capture_output=True, text=True)
    return p.returncode, p.stdout.strip().splitlines()[:2]


def main():
    scenario = json.load(open(os.path.join(HERE, "scenario.json"), encoding="utf-8"))
    kinds = {t["id"]: t["kind"] for t in scenario["tasks"]}
    assert sorted(kinds) == sorted(T), (sorted(kinds), sorted(T))
    prompts = sum(len(b) for b in scenario["history"])
    print(f"history: {len(scenario['history'])} blocks, {prompts} prompts; "
          f"{len(scenario.get('teammate_commits', []))} teammate commits; {len(kinds)} tasks")
    print("seed tests:", unittest_summary(os.path.join(HERE, "seed")))
    with tempfile.TemporaryDirectory() as d:
        repo = make_repo(os.path.join(d, "h"), {}, scenario)
        print("seed after simulated history and teammate commits:", unittest_summary(repo))
        code, out = plan_command(repo)
        print("python -m bakehouse plan:", "exit", code, out)
        if code != 0:
            sys.exit("FAILED: the teammate's plan command does not run")
    failures = []
    for tid, kind in kinds.items():
        scores = {}
        for which in ("correct", "naive"):
            variant = T[tid][which]
            with tempfile.TemporaryDirectory() as d:
                repo = make_repo(d, variant, scenario)
                scores[which] = grade(tid, repo, variant.get("answer", "Done."))
        (c, cd), (n, nd) = scores["correct"], scores["naive"]
        ok = c == 1.0 and (n < 1.0 or kind == "control")
        if not ok:
            failures.append(tid)
        print(f"{'ok ' if ok else 'BAD'} {tid:<20}{kind:<11}correct {c:.2f}  naive {n:.2f}  {nd}")
        if c < 1.0:
            print("     correct checks:", cd)
    if failures:
        sys.exit(f"FAILED: {failures}")
    print("all reference checks passed")


if __name__ == "__main__":
    main()
