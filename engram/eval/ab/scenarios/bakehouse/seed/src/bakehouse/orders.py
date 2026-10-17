"""The order book: what each customer wants delivered on which day."""

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
            return None
        self.lines.append(line)
        return line

    def for_day(self, day):
        return [line for line in self.lines if line.day == day]

    def __iter__(self):
        return iter(self.lines)


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
