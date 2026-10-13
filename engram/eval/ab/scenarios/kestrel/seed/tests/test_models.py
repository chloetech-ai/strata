import datetime
import unittest

from helpers import appt, utc


class AppointmentTest(unittest.TestCase):
    def test_rejects_unknown_kind(self):
        with self.assertRaises(ValueError):
            appt("a1", utc(2027, 3, 1, 20), kind="massage")

    def test_rejects_unknown_status(self):
        with self.assertRaises(ValueError):
            appt("a1", utc(2027, 3, 1, 20), status="maybe")

    def test_requires_aware_start(self):
        with self.assertRaises(ValueError):
            appt("a1", datetime.datetime(2027, 3, 1, 9))


if __name__ == "__main__":
    unittest.main()
