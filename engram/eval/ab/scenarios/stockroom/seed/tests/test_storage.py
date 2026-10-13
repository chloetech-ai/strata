import datetime
import os
import tempfile
import unittest

from stockroom.inventory import Inventory
from stockroom.storage import load_movements, save_movements


class StorageTest(unittest.TestCase):
    def test_roundtrip(self):
        inv = Inventory()
        inv.receive("BR-1042", 100, 12, datetime.date(2026, 9, 1), "PO 4411")
        inv.issue("BR-1042", 7, datetime.date(2026, 9, 2))
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "movements.csv")
            save_movements(inv, path)
            again = load_movements(path)
        self.assertEqual(again.on_hand("BR-1042"), 93)
        self.assertEqual(again.movements[0].unit_cost, 12)
        self.assertEqual(again.movements[0].note, "PO 4411")
        self.assertIsNone(again.movements[1].unit_cost)


if __name__ == "__main__":
    unittest.main()
