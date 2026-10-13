"""Saving and loading a ledger as JSON."""

import datetime
import json

from .ledger import Expense, Ledger


def save(ledger, path):
    data = [
        {
            "id": e.id,
            "date": e.date.isoformat(),
            "payer": e.payer,
            "amount": e.amount,
            "description": e.description,
            "participants": e.participants,
        }
        for e in ledger.expenses
    ]
    with open(path, "w", encoding="utf-8") as f:
        json.dump({"expenses": data}, f, indent=2, sort_keys=True)


def load(path):
    with open(path, encoding="utf-8") as f:
        data = json.load(f)
    ledger = Ledger()
    for d in data["expenses"]:
        ledger.add(Expense(datetime.date.fromisoformat(d["date"]), d["payer"], d["amount"], d["description"], d["participants"], d["id"]))
    return ledger
