import datetime
import unittest

from bakehouse.orders import OrderBook, OrderLine
from bakehouse.production import batches_needed, plan
from bakehouse.products import Catalog, Product

DAY = datetime.date(2026, 10, 20)


class ProductionTest(unittest.TestCase):
    def test_exact_batches(self):
        self.assertEqual(batches_needed(24, 12), 2)

    def test_partial_batch_rounds_up(self):
        self.assertEqual(batches_needed(25, 12), 3)

    def test_plan_adds_up_customers(self):
        catalog = Catalog([Product("BAG", "Baguette", 95, batch_size=12), Product("SOU", "White sourdough", 320, batch_size=10)])
        book = OrderBook(catalog, [
            OrderLine(DAY, "GREENWAY", "BAG", 20),
            OrderLine(DAY, "LUMEN", "BAG", 5),
            OrderLine(DAY, "LUMEN", "SOU", 10),
        ])
        self.assertEqual(plan(book, DAY), {"BAG": (25, 3), "SOU": (10, 1)})


if __name__ == "__main__":
    unittest.main()
