"""Graders for the bakehouse scenario: grade(task_id, repo, answer) -> (score, checks)."""

import csv
import io
import os
import re

from ab_common import checks, read, run_py

# Shared prelude for the snippets run against the repository: finds a function
# by name (in the module the task named, else anywhere in the package) and
# builds catalogs, order books and customers through the public API.
PRELUDE = r'''
import datetime, importlib, json, os, pkgutil, re, tempfile
D = datetime.date
DT = datetime.datetime

def find(name, module):
    try:
        m = importlib.import_module(module)
        if callable(getattr(m, name, None)):
            return getattr(m, name)
    except ImportError:
        pass
    import bakehouse
    for info in pkgutil.iter_modules(bakehouse.__path__):
        if info.name.startswith("_"):
            continue
        try:
            m = importlib.import_module("bakehouse." + info.name)
        except Exception:
            continue
        if callable(getattr(m, name, None)):
            return getattr(m, name)
    raise ImportError(name)

from bakehouse.products import Catalog, Product
from bakehouse.orders import OrderBook, OrderLine
from bakehouse.customers import Customer
from bakehouse.invoice import Invoice, InvoiceLine

def prod(code, name, price=100, batch=1, dough=100, temp=220, allergens=()):
    return Product(code=code, name=name, price=price, batch_size=batch, dough_g=dough, bake_temp=temp, allergens=tuple(allergens))

def cust(code, name, address, postcode, seq):
    return Customer(code=code, name=name, address=address, postcode=postcode, route_seq=seq)

def book(catalog, lines):
    b = OrderBook(catalog)
    for day, customer, product, qty in lines:
        b.add(OrderLine(day=day, customer=customer, product=product, qty=qty))
    return b

def read_file(path):
    data = open(path, "rb").read()
    for enc in ("utf-8-sig", "cp1252"):
        try:
            return data.decode(enc)
        except UnicodeDecodeError:
            pass
    return data.decode("latin-1")

def as_text(out):
    if isinstance(out, bytes):
        out = out.decode("utf-8", "replace")
    if isinstance(out, str):
        if len(out) < 400 and "\n" not in out and os.path.isfile(out):
            return read_file(out)
        return out
    if hasattr(out, "__fspath__"):
        return read_file(out)
    if isinstance(out, (list, tuple)):
        return "\n".join(as_text(x) if isinstance(x, (str, bytes)) else (" ".join(map(str, x)) if isinstance(x, (list, tuple)) else str(x)) for x in out)
    return str(out)

def written(fn, args, name):
    """Calls fn(*args, path) with a fresh path; returns the text it wrote."""
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, name)
        ret = fn(*args, p)
        if os.path.isfile(p):
            return read_file(p)
        if isinstance(ret, (str, bytes)) or hasattr(ret, "__fspath__"):
            return as_text(ret)
        found = [os.path.join(d, f) for f in os.listdir(d)]
        return read_file(found[0]) if found else ""

ORDER_CUSTOMERS = {
    "LUMEN": ("Café Lumen", "14 Mill Lane", "LK1 2AB", 1),
    "HARBOUR": ("Harbour Deli", "3 Quay Street", "LK2 7QS", 2),
    "OLDMILL": ("The Old Mill Tearoom", "Old Mill Yard", "LK3 1MY", 3),
    "GREENWAY": ("Greenway Grocers", "88 High Street", "LK1 4HS", 4),
    "AIDAN": ("St Aidan's Primary School", "School Road", "LK4 9SA", 5),
}

def customers(*codes):
    return {c: cust(c, *ORDER_CUSTOMERS[c]) for c in codes}
'''


def _py(repo, body):
    return run_py(repo, PRELUDE + body)


def _num(n):
    """Regex for an integer written plainly or with a thousands separator."""
    s = str(n)
    if len(s) > 3:
        s = s[:-3] + r"[,\s .']?" + s[-3:]
    return r"(?<![\d.,])" + s + r"(?!\d)"


# ------------------------------------------------------------ revised facts


def grade_delivery_charge(repo, answer):
    r = _py(repo, r'''
f = find("delivery_charge", "bakehouse.invoice")
cases = [["LUMEN", 2400], ["LUMEN", 3000], ["HARBOUR", 3999], ["LUMEN", 4000], ["GREENWAY", 12000], ["AIDAN", 1500], ["AIDAN", 3000]]
out = []
for c, s in cases:
    try:
        out.append(float(f(c, s)))
    except Exception as e:
        out.append(repr(e)[:80])
print(json.dumps(out))
''')
    if "error" in r:
        return 0.0, r
    o = r
    return checks(
        charge_is_4_50=o[0] == 450,
        threshold_is_40_pounds=o[1] == 450 and o[2] == 450 and o[3] == 0 and o[4] == 0,
        school_never_pays=o[5] == 0 and o[6] == 0,
    )


def grade_price_rise(repo, answer):
    r = _py(repo, r'''
cat = Catalog([prod("P1", "Baguette", 250), prod("P2", "White roll", 120), prod("P3", "Seeded rye", 395),
               prod("P4", "Celebration loaf", 1010), prod("P5", "Croissant", 87), prod("P6", "Brioche bun", 33)])
out = find("new_prices", "bakehouse.pricing")(cat)
if not isinstance(out, dict):
    out = dict(out)
print(json.dumps({str(getattr(k, "code", k)).strip().upper(): float(v) for k, v in out.items()}))
''')
    if "error" in r:
        return 0.0, r
    # 5% then up to the next 5p (4% to the nearest penny: 260, 125, 411, 1050, 90, 34)
    want = {"P1": 265, "P2": 130, "P3": 415, "P4": 1065, "P5": 95, "P6": 35}
    return checks(
        every_product=set(r) == set(want),
        whole_5p_steps=bool(r) and all(v % 5 == 0 for v in r.values()),
        five_percent_rounded_up=all(r.get(k) == v for k, v in want.items()),
    )


def grade_bake_sheet(repo, answer):
    r = _py(repo, r'''
cat = Catalog([
    prod("BAG", "Baguette", 95, 12, 350, 250, ["wheat"]),
    prod("SOU", "White sourdough", 320, 10, 800, 240, ["wheat"]),
    prod("RYE", "Seeded rye", 360, 8, 900, 230, ["rye", "wheat", "sesame"]),
    prod("CRO", "Croissant", 110, 24, 95, 200, ["wheat", "milk", "eggs"]),
    prod("CIN", "Cinnamon bun", 140, 20, 110, 190, ["wheat", "milk", "eggs"]),
    prod("WRL", "White roll", 35, 30, 90, 220, ["wheat"]),
])
day = D(2026, 11, 17)
b = book(cat, [(day, "LUMEN", "CRO", 24), (day, "GREENWAY", "BAG", 24), (day, "LUMEN", "SOU", 10),
               (day, "OLDMILL", "CIN", 20), (day, "HARBOUR", "RYE", 8), (day, "AIDAN", "CRO", 24),
               (day, "OLDMILL", "SOU", 10), (D(2026, 11, 18), "AIDAN", "WRL", 60)])
print(json.dumps({"text": as_text(find("bake_sheet", "bakehouse.production")(b, day))}))
''')
    if "error" in r:
        return 0.0, r
    text = r["text"]
    low = text.lower()
    # hottest first; quantities are whole batches, so units x dough_g is unambiguous
    rows = [("Baguette", 8400), ("White sourdough", 16000), ("Seeded rye", 7200), ("Croissant", 4560), ("Cinnamon bun", 2200)]
    pos = [low.find(n.lower()) for n, _ in rows]
    lines = text.splitlines()

    def line_of(name):
        return next((l for l in lines if name.lower() in l.lower()), "")

    grams = all(re.search(_num(g), line_of(n)) for n, g in rows)
    kilos = re.search(r"(?i)\d+[.,]\d+\s*kg|\bkg\b|kilo", text)
    return checks(
        lists_the_days_products=all(p >= 0 for p in pos) and "white roll" not in low,
        hottest_oven_first=all(p >= 0 for p in pos) and pos == sorted(pos),
        dough_in_whole_grams=grams and not kilos,
    )


def grade_crate_labels(repo, answer):
    r = _py(repo, r'''
cat = Catalog([
    prod("RYL", "Seeded rye sourdough, large loaf", 420, 8, 900, 230, ["rye", "sesame", "wheat"]),
    prod("BRO", "Brötchen", 40, 30, 85, 230, ["wheat"]),
    prod("CRO", "Croissant", 110, 24, 95, 200, ["eggs", "milk", "wheat"]),
])
day = D(2026, 11, 19)
b = book(cat, [(day, "LUMEN", "RYL", 12), (day, "LUMEN", "BRO", 30), (day, "GREENWAY", "CRO", 48),
               (day, "GREENWAY", "RYL", 6), (D(2026, 11, 20), "LUMEN", "CRO", 5)])
out = find("crate_labels", "bakehouse.labels")(b, customers("LUMEN", "GREENWAY", "HARBOUR"), day)
if isinstance(out, dict):
    out = list(out.values())
if isinstance(out, str):
    out = [x for x in re.split(r"\n\s*\n", out) if x.strip()]
labels = []
for x in out:
    if isinstance(x, (list, tuple)):
        labels.append("\n".join(map(str, x)))
    else:
        labels.append(as_text(x))
print(json.dumps({"labels": labels}))
''')
    if "error" in r:
        return 0.0, r
    labels = r["labels"]
    name = "seeded rye sourdough, large loaf"

    def label(cust_word, prod_word):
        return next((l for l in labels if cust_word in l.lower() and prod_word in l.lower()), "")

    lumen_rye, green_rye, green_cro = label("lumen", "rye"), label("greenway", "rye"), label("greenway", "croissant")

    def qty_and_full_name(lbl, qty):
        return any(name in line.lower() and re.search(rf"(?<!\d){qty}(?!\d)", line) for line in lbl.splitlines())

    return checks(
        one_label_per_product_line=len(labels) == 4,
        plain_ascii=bool(labels) and all(ord(ch) < 128 for l in labels for ch in l),
        allergens_in_capitals=all(re.search(rf"\b{a}\b", lumen_rye) for a in ("RYE", "SESAME", "WHEAT"))
        and all(re.search(rf"\b{a}\b", green_cro) for a in ("EGGS", "MILK", "WHEAT")),
        lines_fit_48=bool(labels) and all(len(line) <= 48 for l in labels for line in l.splitlines()),
        name_not_cut_at_32=qty_and_full_name(lumen_rye, 12) and qty_and_full_name(green_rye, 6),
    )


# ------------------------------------------------------------ stale facts


def grade_extend_cutoff(repo, answer):
    r = _py(repo, r'''
f = find("is_on_time", "bakehouse.orders")
day = D(2026, 11, 18)
prev = day - datetime.timedelta(days=1)
def at(d, h, m):
    return DT.combine(d, datetime.time(h, m))
cases = {"1329": at(prev, 13, 29), "1100": at(prev, 11, 0), "2days": at(day - datetime.timedelta(days=2), 16, 0),
         "1331": at(prev, 13, 31), "1415": at(prev, 14, 15), "1445": at(prev, 14, 45)}
print(json.dumps({k: bool(f(v, day)) for k, v in cases.items()}))
''')
    if "error" in r:
        return 0.0, r
    # current cutoff 13:00 (Sam's commit) + 30 min = 13:30; trusting block 2's 14:00 gives 14:30
    return checks(
        open_until_13_30=r["1329"] and r["1100"] and r["2days"],
        closed_after_13_30=not r["1331"] and not r["1415"] and not r["1445"],
    )


def grade_next_delivery_day(repo, answer):
    r = _py(repo, r'''
f = find("next_delivery_day", "bakehouse.orders")
print(json.dumps({c: str(f(D.fromisoformat(c)))[:10] for c in ["2026-11-07", "2026-11-08", "2026-11-09", "2026-11-11", "2026-11-14"]}))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        delivers_on_sundays=r["2026-11-07"] == "2026-11-08" and r["2026-11-14"] == "2026-11-15",
        closed_on_mondays=r["2026-11-08"] == "2026-11-10" and r["2026-11-09"] == "2026-11-10",
        ordinary_days=r["2026-11-11"] == "2026-11-12",
    )


def grade_relief_baker_note(repo, answer):
    a = answer or ""
    return checks(
        current_command=re.search(r"(?i)python3?\s+-m\s+bakehouse\s+plan\b", a) is not None,
        names_the_day=re.search(r"--day\b", a) is not None,
    )


# ------------------------------------------------------------ inference hops


def _rows(text, delimiter):
    return [row for row in csv.reader(io.StringIO(text), delimiter=delimiter) if any(c.strip() for c in row)]


def grade_owner_weekly_file(repo, answer):
    r = _py(repo, r'''
cat = Catalog([prod("SOU", "White sourdough", 320), prod("CRO", "Croissant", 110), prod("BAG", "Baguette", 95)])
b = book(cat, [(D(2026, 11, 17), "LUMEN", "SOU", 5), (D(2026, 11, 19), "LUMEN", "CRO", 12),
               (D(2026, 11, 18), "GREENWAY", "BAG", 20), (D(2026, 11, 22), "GREENWAY", "SOU", 3),
               (D(2026, 11, 23), "HARBOUR", "SOU", 10), (D(2026, 11, 15), "AIDAN", "CRO", 10)])
print(json.dumps({"text": written(find("week_totals_file", "bakehouse.reports"), (b, D(2026, 11, 16)), "week.csv")}))
''')
    if "error" in r:
        return 0.0, r
    text = r["text"]
    lines = [l for l in text.splitlines() if l.strip()]

    def line_of(word):
        return next((l for l in lines if word in l.lower()), "")

    lumen, green = line_of("lumen"), line_of("greenway")
    # LUMEN 5x320 + 12x110 = 29.20, GREENWAY 20x95 + 3x320 = 28.60 (Harbour and Aidan are outside the week)
    return checks(
        semicolon_separated=bool(lumen and green) and all(len(l.split(";")) >= 2 for l in (lumen, green)),
        decimal_comma=re.search(r"(?<![\d.,])29,20?(?!\d)", lumen) is not None
        and re.search(r"(?<![\d.,])28,60?(?!\d)", green) is not None and not re.search(r"29\.20?(?!\d)", text),
        only_that_week=not re.search(r"(?<![\d.,])(32[.,]00|11[.,]00|3200|1100)(?![\d])", text),
    )


def grade_vat_export(repo, answer):
    r = _py(repo, r'''
invs = [
    Invoice(customer="LUMEN", day=D(2026, 11, 3), lines=[InvoiceLine(product="SOU", qty=5, unit_price=320), InvoiceLine(product="CRO", qty=12, unit_price=110)]),
    Invoice(customer="GREENWAY", day=D(2026, 11, 4), lines=[InvoiceLine(product="BAG", qty=20, unit_price=95)]),
    Invoice(customer="AIDAN", day=D(2026, 12, 1), lines=[InvoiceLine(product="WRL", qty=60, unit_price=35)]),
]
print(json.dumps({"text": written(find("export_invoices", "bakehouse.invoice"), (invs,), "vat.csv")}))
''')
    if "error" in r:
        return 0.0, r
    text = r["text"]
    rows = _rows(text, ",")
    want = {"LUMEN": ("03/11/2026", "29.20"), "GREENWAY": ("04/11/2026", "19.00"), "AIDAN": ("01/12/2026", "21.00")}

    def row_of(code):
        return next(([c.strip() for c in row] for row in rows if any(code in c.upper() for c in row)), [])

    def money(cell):
        return cell.replace("£", "").strip()

    return checks(
        comma_csv_with_header=bool(rows) and not re.search(r"\d", ",".join(rows[0])) and ";" not in text
        and len(rows) >= 4 and all(len(row) >= 3 for row in rows[:4]),
        dates_dd_mm_yyyy=all(d in row_of(c) for c, (d, _) in want.items()),
        pounds_two_decimals=all(any(money(cell) == amt for cell in row_of(c)) for c, (_, amt) in want.items()),
    )


def grade_van_list(repo, answer):
    r = _py(repo, r'''
cat = Catalog([prod("WRL", "White roll", 35), prod("CRO", "Croissant", 110), prod("SWL", "Sandwich loaf", 180),
               prod("CIN", "Cinnamon bun", 140), prod("SOU", "White sourdough", 320), prod("RYE", "Seeded rye", 360)])
day = D(2026, 11, 18)
b = book(cat, [(day, "AIDAN", "WRL", 60), (day, "LUMEN", "CRO", 24), (day, "GREENWAY", "SWL", 32),
               (day, "OLDMILL", "CIN", 20), (day, "LUMEN", "SOU", 6), (D(2026, 11, 19), "HARBOUR", "RYE", 8)])
cs = customers("LUMEN", "HARBOUR", "OLDMILL", "GREENWAY", "AIDAN")
print(json.dumps({"text": written(find("drops_file", "bakehouse.delivery"), (b, cs, day), "drops.csv")}))
''')
    if "error" in r:
        return 0.0, r
    rows = _rows(r["text"], ",")
    header = [c.strip() for c in rows[0]] if rows else []
    data = rows[1:]
    codes = {"LK1 4HS": "GREENWAY", "LK1 2AB": "LUMEN", "LK3 1MY": "OLDMILL", "LK4 9SA": "AIDAN", "LK2 7QS": "HARBOUR"}

    def who(row):
        return next((c for pc, c in codes.items() if any(pc in cell for cell in row)), "?")

    order = [who(row) for row in data]
    return checks(
        stopwise_header=header == ["Name", "Address", "Postcode", "Notes"],
        one_row_per_drop=len(data) == 4 and "HARBOUR" not in order,
        greenway_first=order[:1] == ["GREENWAY"],
        then_route_order=[c for c in order if c != "GREENWAY"] == ["LUMEN", "OLDMILL", "AIDAN"],
    )


def grade_school_order(repo, answer):
    text = read(repo, os.path.join("data", "orders.csv"))
    lines = []
    for row in csv.DictReader(io.StringIO(text)):
        row = {str(k).strip().lower(): str(v or "").strip() for k, v in row.items() if k}
        if "2026-11-20" in row.get("date", "") and "AIDAN" in row.get("customer", "").upper():
            lines.append((row.get("product", "").upper(), row.get("qty", "")))
    return checks(
        enters_rolls_and_loaves=("WRL", "60") in lines and ("SWL", "6") in lines,
        leaves_out_the_bakewell_slices=not any(p == "BKW" or "BAKEWELL" in p for p, _ in lines),
        says_why=re.search(r"(?i)\bnuts?\b|nut[- ]free|almond|allerg", answer or "") is not None,
    )


# ------------------------------------------------------------ recall


def grade_returns_credit(repo, answer):
    r = _py(repo, r'''
cat = Catalog([prod("SWL", "Sandwich loaf", 180, 16, 850, 220, ["wheat", "soya"]), prod("WRL", "White roll", 35),
               prod("SOU", "White sourdough", 320)])
f = find("returns_credit", "bakehouse.invoice")
cases = [["GREENWAY", "SWL", 40, 3], ["GREENWAY", "SWL", 40, 7], ["GREENWAY", "SWL", 30, 5], ["GREENWAY", "SOU", 20, 2], ["LUMEN", "SWL", 40, 3]]
out = []
for c in cases:
    try:
        out.append(float(f(cat, *c)))
    except ValueError:
        out.append("ValueError")
    except Exception as e:
        out.append(type(e).__name__)
print(json.dumps(out))
''')
    if "error" in r:
        return 0.0, r
    nothing = (0, "ValueError")
    return checks(
        half_price=r[0] == 270,
        at_most_a_tenth_of_the_delivery=r[1] == 360 and r[2] == 270,
        sandwich_loaves_only=r[3] in nothing,
        greenway_only=r[4] in nothing,
    )


# ------------------------------------------------------------ where and why


def grade_pence_why(repo, answer):
    a = answer or ""
    return checks(
        says_where=re.search(r"(?i)invoice\.py|Invoice\s*\.\s*total", a) is not None,
        says_why=re.search(r"(?i)1\.37|HMRC|\bPriya\b|(old|previous|former) spreadsheet|VAT return (was|came out|ended up) (\S+ )?(off|wrong|out)", a) is not None,
    )


def grade_unknown_code_why(repo, answer):
    a = answer or ""
    return checks(
        says_where=re.search(r"(?i)orders\.py|OrderBook\s*\.\s*add", a) is not None,
        says_why=re.search(r"(?i)Greenway|cash[- ]and[- ]carry", a) is not None,
    )


# ------------------------------------------------------------ controls


def _mentions_in_tests(repo, name):
    tests = os.path.join(repo, "tests")
    return os.path.isdir(tests) and any(name in read(repo, os.path.join("tests", f)) for f in os.listdir(tests) if f.endswith(".py"))


def grade_easter_sunday(repo, answer):
    r = _py(repo, r'''
f = find("easter_sunday", "bakehouse.seasons")
print(json.dumps({y: str(f(y))[:10] for y in [1943, 1961, 2000, 2008, 2019, 2024, 2025, 2026, 2038, 2100, 2160]}))
''')
    if "error" in r:
        return 0.0, r
    want = {"1943": "1943-04-25", "1961": "1961-04-02", "2000": "2000-04-23", "2008": "2008-03-23", "2019": "2019-04-21",
            "2024": "2024-03-31", "2025": "2025-04-20", "2026": "2026-04-05", "2038": "2038-04-25", "2100": "2100-03-28",
            "2160": "2160-03-23"}
    return checks(dates=r == want, adds_test=_mentions_in_tests(repo, "easter_sunday"))


def grade_baker_percentages(repo, answer):
    r = _py(repo, r'''
f = find("baker_percentages", "bakehouse.recipes")
a = f({"strong white flour": 800, "Wholemeal Flour": 200, "water": 680, "salt": 20, "levain": 150})
b = f({"Rye flour": 300, "water": 251, "salt": 7})
try:
    f({"water": 100, "salt": 2})
    c = "no error"
except ValueError:
    c = "ValueError"
except Exception as e:
    c = type(e).__name__
print(json.dumps({"a": {k: float(v) for k, v in a.items()}, "b": {k: float(v) for k, v in b.items()}, "c": c}))
''')
    if "error" in r:
        return 0.0, r
    want_a = {"strong white flour": 80.0, "Wholemeal Flour": 20.0, "water": 68.0, "salt": 2.0, "levain": 15.0}
    want_b = {"Rye flour": 100.0, "water": 83.7, "salt": 2.3}

    def close(got, want):
        return set(got) == set(want) and all(abs(got[k] - v) < 0.051 for k, v in want.items())

    return checks(
        percentages=close(r["a"], want_a) and close(r["b"], want_b),
        no_flour_raises=r["c"] == "ValueError",
        adds_test=_mentions_in_tests(repo, "baker_percentages"),
    )


GRADERS = {
    "delivery_charge": grade_delivery_charge,
    "price_rise": grade_price_rise,
    "bake_sheet": grade_bake_sheet,
    "crate_labels": grade_crate_labels,
    "extend_cutoff": grade_extend_cutoff,
    "next_delivery_day": grade_next_delivery_day,
    "relief_baker_note": grade_relief_baker_note,
    "owner_weekly_file": grade_owner_weekly_file,
    "vat_export": grade_vat_export,
    "van_list": grade_van_list,
    "school_order": grade_school_order,
    "returns_credit": grade_returns_credit,
    "pence_why": grade_pence_why,
    "unknown_code_why": grade_unknown_code_why,
    "easter_sunday": grade_easter_sunday,
    "baker_percentages": grade_baker_percentages,
}


def grade(task_id, repo, answer):
    return GRADERS[task_id](repo, answer or "")
