"""Expenses and balances."""

from dataclasses import dataclass, field
import datetime
import itertools

_ids = itertools.count(1)


@dataclass
class Expense:
    date: datetime.date
    payer: str
    amount: int  # cents
    description: str
    participants: list
    id: int = field(default_factory=lambda: next(_ids))


class Ledger:
    def __init__(self):
        self.expenses = []

    def add(self, expense):
        self.expenses.append(expense)
        return expense

    def split_equal(self, amount, participants, payer=None):
        """Each participant's share of `amount` cents."""
        n = len(participants)
        share = amount // n
        return {p: share for p in participants}

    def balance_of(self, person):
        """What `person` paid minus what they owe, in cents."""
        paid = sum(e.amount for e in self.expenses if e.payer == person)
        owed = 0
        for e in self.expenses:
            if person in e.participants:
                owed += self.split_equal(e.amount, e.participants, e.payer)[person]
        return paid - owed

    def people(self):
        names = set()
        for e in self.expenses:
            names.add(e.payer)
            names.update(e.participants)
        return sorted(names)
