"""Stock movements and quantities on hand."""

from dataclasses import dataclass
import datetime
from typing import Optional


@dataclass
class Movement:
    sku: str
    qty: int  # positive: goods in, negative: goods out
    date: datetime.date
    unit_cost: Optional[int] = None  # cents; set for receipts
    note: str = ""


class Inventory:
    """An append-only log of stock movements."""

    def __init__(self, catalog=None):
        self.catalog = catalog
        self.movements = []

    def _record(self, movement):
        self.movements.append(movement)
        return movement

    def receive(self, sku, qty, unit_cost, date=None, note=""):
        """Goods in from a supplier, `unit_cost` in cents."""
        if qty <= 0:
            raise ValueError("quantity must be positive")
        return self._record(Movement(sku, qty, date or datetime.date.today(), unit_cost, note))

    def issue(self, sku, qty, date=None, note=""):
        """Goods out, e.g. a sale."""
        if qty <= 0:
            raise ValueError("quantity must be positive")
        return self._record(Movement(sku, -qty, date or datetime.date.today(), None, note))

    def on_hand(self, sku):
        return sum(m.qty for m in self.movements if m.sku == sku)

    def movements_for(self, sku):
        return [m for m in self.movements if m.sku == sku]
