"""Prints the bake plan for one delivery day.

    python scripts/print_plan.py --day 2026-10-20
"""

import argparse
import datetime
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "src"))

from bakehouse.orders import load_orders  # noqa: E402
from bakehouse.production import plan  # noqa: E402
from bakehouse.products import load_catalog  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Print the bake plan for a delivery day.")
    parser.add_argument("--day", required=True, type=datetime.date.fromisoformat, help="YYYY-MM-DD")
    args = parser.parse_args()
    catalog = load_catalog(os.path.join(ROOT, "data", "products.csv"))
    book = load_orders(os.path.join(ROOT, "data", "orders.csv"), catalog)
    for code, (units, batches) in plan(book, args.day).items():
        print(code, units)


if __name__ == "__main__":
    main()
