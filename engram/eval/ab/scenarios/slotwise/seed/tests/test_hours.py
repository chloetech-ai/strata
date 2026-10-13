import datetime
import unittest

from slotwise.hours import is_open_between, opening_hours

DT = datetime.datetime


class HoursTest(unittest.TestCase):
    def test_weekday(self):
        self.assertTrue(is_open_between(DT(2026, 9, 7, 7, 30), DT(2026, 9, 7, 8, 30)))  # Monday
        self.assertFalse(is_open_between(DT(2026, 9, 7, 18, 30), DT(2026, 9, 7, 19, 15)))

    def test_friday_closes_early(self):
        self.assertFalse(is_open_between(DT(2026, 9, 11, 16, 30), DT(2026, 9, 11, 17, 15)))

    def test_closed_on_sunday(self):
        self.assertIsNone(opening_hours(datetime.date(2026, 9, 13)))


if __name__ == "__main__":
    unittest.main()
