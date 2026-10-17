import datetime
import os
import tempfile
import unittest

from bakehouse import config
from bakehouse.orders import OrderBook, OrderLine, is_delivery_day, is_on_time, load_orders, save_orders
from bakehouse.products import Catalog, Product

DAY = datetime.date(2026, 10, 20)


def catalog():
    return Catalog([Product("SOU", "White sourdough", 320, 10), Product("CRO", "Croissant", 110, 24)])


class OrderBookTest(unittest.TestCase):
    def test_for_day(self):
        book = OrderBook(catalog(), [
            OrderLine(DAY, "LUMEN", "SOU", 6),
            OrderLine(DAY + datetime.timedelta(days=1), "LUMEN", "CRO", 24),
        ])
        self.assertEqual([line.product for line in book.for_day(DAY)], ["SOU"])

    def test_quantity_must_be_positive(self):
        with self.assertRaises(ValueError):
            OrderBook(catalog()).add(OrderLine(DAY, "LUMEN", "SOU", 0))

    def test_save_and_load(self):
        book = OrderBook(catalog(), [OrderLine(DAY, "LUMEN", "SOU", 6), OrderLine(DAY, "AIDAN", "CRO", 48)])
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "orders.csv")
            save_orders(book, path)
            again = load_orders(path, catalog())
        self.assertEqual(sorted((l.customer, l.qty) for l in again), [("AIDAN", 48), ("LUMEN", 6)])


class CutoffTest(unittest.TestCase):
    def test_well_before_the_cutoff(self):
        placed = datetime.datetime.combine(DAY - datetime.timedelta(days=2), config.ORDER_CUTOFF)
        self.assertTrue(is_on_time(placed, DAY))

    def test_on_the_delivery_day_is_too_late(self):
        self.assertFalse(is_on_time(datetime.datetime.combine(DAY, datetime.time(6, 0)), DAY))

    def test_closed_days(self):
        for offset in range(7):
            day = DAY + datetime.timedelta(days=offset)
            self.assertEqual(is_delivery_day(day), day.weekday() not in config.CLOSED_WEEKDAYS)


if __name__ == "__main__":
    unittest.main()
