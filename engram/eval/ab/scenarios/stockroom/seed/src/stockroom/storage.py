"""Saving and loading the movement log as CSV."""

import csv
import datetime

from .inventory import Inventory, Movement

FIELDS = ["date", "sku", "qty", "unit_cost", "note"]


def save_movements(inventory, path):
    with open(path, "w", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=FIELDS)
        writer.writeheader()
        for m in inventory.movements:
            writer.writerow({
                "date": m.date.isoformat(),
                "sku": m.sku,
                "qty": m.qty,
                "unit_cost": "" if m.unit_cost is None else m.unit_cost,
                "note": m.note,
            })


def load_movements(path, catalog=None):
    inventory = Inventory(catalog)
    with open(path, newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            cost = int(row["unit_cost"]) if row["unit_cost"] else None
            date = datetime.date.fromisoformat(row["date"])
            inventory.movements.append(Movement(row["sku"], int(row["qty"]), date, cost, row["note"]))
    return inventory
