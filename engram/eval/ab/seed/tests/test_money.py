import unittest

from tally.money import format_amount, parse_amount


class MoneyTest(unittest.TestCase):
    def test_parse(self):
        self.assertEqual(parse_amount("12.50"), 1250)
        self.assertEqual(parse_amount("12"), 1200)
        self.assertEqual(parse_amount("0.5"), 50)
        self.assertEqual(parse_amount("-3.25"), -325)

    def test_format_roundtrip(self):
        for cents in (0, 5, 1250, 100000):
            self.assertEqual(parse_amount(format_amount(cents).split()[-1]), cents)


if __name__ == "__main__":
    unittest.main()
