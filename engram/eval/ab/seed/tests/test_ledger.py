import datetime
import unittest

from tally.ledger import Expense, Ledger


def expense(payer, amount, participants, desc="x"):
    return Expense(datetime.date(2026, 3, 14), payer, amount, desc, participants)


class LedgerTest(unittest.TestCase):
    def test_split_equal_even(self):
        shares = Ledger().split_equal(900, ["ana", "ben", "cy"], "ana")
        self.assertEqual(shares, {"ana": 300, "ben": 300, "cy": 300})

    def test_split_equal_keeps_every_cent(self):
        shares = Ledger().split_equal(1000, ["ana", "ben", "cy"], "ben")
        self.assertEqual(sum(shares.values()), 1000)

    def test_balances_sum_to_zero(self):
        ledger = Ledger()
        ledger.add(expense("ana", 3000, ["ana", "ben", "cy"]))
        ledger.add(expense("ben", 1000, ["ben", "cy"]))
        self.assertEqual(sum(ledger.balance_of(p) for p in ledger.people()), 0)


if __name__ == "__main__":
    unittest.main()
