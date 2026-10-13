import datetime
import unittest

from slotwise.model import Appointment
from slotwise.schedule import Schedule


def at(hour, minute=0, therapist="lena", treatment="standard", client="Kim Weber"):
    return Appointment(therapist, client, datetime.datetime(2026, 9, 8, hour, minute), treatment)


class ScheduleTest(unittest.TestCase):
    def test_book(self):
        s = Schedule()
        a = s.book(at(9))
        self.assertIs(s.get(a.id), a)

    def test_same_start_is_taken(self):
        s = Schedule()
        s.book(at(9))
        with self.assertRaises(ValueError):
            s.book(at(9, client="Jo Li"))

    def test_overlap_from_before_is_taken(self):
        s = Schedule()
        s.book(at(9))
        with self.assertRaises(ValueError):
            s.book(at(8, 30, client="Jo Li"))

    def test_other_therapist_is_free(self):
        s = Schedule()
        s.book(at(9))
        s.book(at(9, therapist="marco", client="Jo Li"))
        self.assertEqual(len(s.appointments), 2)

    def test_outside_opening_hours(self):
        s = Schedule()
        with self.assertRaises(ValueError):
            s.book(at(18, 30))

    def test_appointments_for_day_in_order(self):
        s = Schedule()
        s.book(at(14))
        s.book(at(8))
        s.book(at(10, therapist="marco"))
        self.assertEqual([a.start.hour for a in s.appointments_for("lena", datetime.date(2026, 9, 8))], [8, 14])


if __name__ == "__main__":
    unittest.main()
