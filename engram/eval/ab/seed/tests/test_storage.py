import datetime
import os
import tempfile
import unittest

from tally.ledger import Expense, Ledger
from tally.storage import load, save


class StorageTest(unittest.TestCase):
    def test_roundtrip(self):
        ledger = Ledger()
        ledger.add(Expense(datetime.date(2026, 3, 1), "ana", 1999, "dinner", ["ana", "ben"]))
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "ledger.json")
            save(ledger, path)
            again = load(path)
        self.assertEqual(again.expenses[0].amount, 1999)
        self.assertEqual(again.expenses[0].participants, ["ana", "ben"])


if __name__ == "__main__":
    unittest.main()
