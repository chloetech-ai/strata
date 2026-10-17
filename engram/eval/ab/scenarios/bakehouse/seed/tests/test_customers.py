import os
import unittest

from bakehouse.customers import load_customers

DATA = os.path.join(os.path.dirname(__file__), "..", "data")


class CustomersTest(unittest.TestCase):
    def test_load_customers(self):
        customers = load_customers(os.path.join(DATA, "customers.csv"))
        self.assertEqual(customers["LUMEN"].postcode, "LK1 2AB")
        self.assertEqual(customers["GREENWAY"].route_seq, 4)


if __name__ == "__main__":
    unittest.main()
