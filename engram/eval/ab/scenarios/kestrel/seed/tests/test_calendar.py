import unittest

from helpers import appt, utc
from kestrel.calendar import Calendar


class CalendarTest(unittest.TestCase):
    def test_all_in_start_order(self):
        cal = Calendar([appt("a2", utc(2027, 3, 1, 22)), appt("a1", utc(2027, 3, 2, 1)), appt("a3", utc(2027, 3, 1, 20))])
        self.assertEqual([a.id for a in cal.all()], ["a3", "a2", "a1"])

    def test_duplicate_id(self):
        cal = Calendar([appt("a1", utc(2027, 3, 1, 20))])
        with self.assertRaises(ValueError):
            cal.add(appt("a1", utc(2027, 3, 1, 21)))

    def test_for_therapist(self):
        cal = Calendar([appt("a1", utc(2027, 3, 1, 20), therapist="T1"), appt("a2", utc(2027, 3, 1, 21), therapist="T2")])
        self.assertEqual([a.id for a in cal.for_therapist("T2")], ["a2"])

    def test_booked(self):
        cal = Calendar([appt("a1", utc(2027, 3, 1, 20)), appt("a2", utc(2027, 3, 1, 21), status="cancelled")])
        self.assertEqual([a.id for a in cal.booked()], ["a1"])


if __name__ == "__main__":
    unittest.main()
