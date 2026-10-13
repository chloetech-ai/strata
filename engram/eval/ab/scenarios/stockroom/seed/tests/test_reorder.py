import unittest

from stockroom.inventory import Inventory
from stockroom.items import Catalog, Item
from stockroom.reorder import needs_reorder


def make_inventory():
    catalog = Catalog([
        Item("BR-1", "Bolt", "A-01-1", reorder_point=10),
        Item("BR-2", "Nut", "A-01-2", reorder_point=10),
        Item("BR-3", "Washer", "A-01-3", reorder_point=10),
    ])
    inv = Inventory(catalog)
    inv.receive("BR-1", 5, 10)
    inv.receive("BR-2", 10, 10)
    inv.receive("BR-3", 50, 10)
    return inv


class ReorderTest(unittest.TestCase):
    def test_below_reorder_point(self):
        self.assertIn("BR-1", needs_reorder(make_inventory()))

    def test_at_reorder_point(self):
        self.assertIn("BR-2", needs_reorder(make_inventory()))

    def test_enough_stock(self):
        self.assertNotIn("BR-3", needs_reorder(make_inventory()))


if __name__ == "__main__":
    unittest.main()
