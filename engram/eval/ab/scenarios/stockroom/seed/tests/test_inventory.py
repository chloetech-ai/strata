import datetime
import unittest

from stockroom.inventory import Inventory

DAY = datetime.date(2026, 9, 1)


class InventoryTest(unittest.TestCase):
    def test_receive_and_issue(self):
        inv = Inventory()
        inv.receive("BR-1042", 100, 12, DAY)
        inv.issue("BR-1042", 30, DAY)
        self.assertEqual(inv.on_hand("BR-1042"), 70)
        self.assertEqual(inv.on_hand("BR-9999"), 0)

    def test_quantities_must_be_positive(self):
        inv = Inventory()
        with self.assertRaises(ValueError):
            inv.receive("BR-1042", 0, 12)
        with self.assertRaises(ValueError):
            inv.issue("BR-1042", -5)

    def test_movements_for(self):
        inv = Inventory()
        inv.receive("BR-1042", 10, 12, DAY)
        inv.receive("BR-2210", 5, 30, DAY)
        inv.issue("BR-1042", 4, DAY)
        self.assertEqual([m.qty for m in inv.movements_for("BR-1042")], [10, -4])


if __name__ == "__main__":
    unittest.main()
