import datetime
import unittest

from slotwise.model import Appointment


class AppointmentTest(unittest.TestCase):
    def test_end_and_price(self):
        a = Appointment("lena", "Kim Weber", datetime.datetime(2026, 9, 7, 9, 0), "initial")
        self.assertEqual(a.end, datetime.datetime(2026, 9, 7, 10, 0))
        self.assertEqual(a.price, 14500)

    def test_ids_are_unique(self):
        start = datetime.datetime(2026, 9, 7, 9, 0)
        self.assertNotEqual(Appointment("lena", "A", start).id, Appointment("lena", "B", start).id)


if __name__ == "__main__":
    unittest.main()
