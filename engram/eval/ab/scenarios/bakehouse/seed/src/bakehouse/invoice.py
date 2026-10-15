"""Invoices. All amounts are integer pence."""

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
