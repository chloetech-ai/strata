import datetime
import os
import tempfile
import unittest

from slotwise.model import Appointment
from slotwise.schedule import Schedule
from slotwise.storage import load, save


class StorageTest(unittest.TestCase):
    def test_roundtrip(self):
        s = Schedule()
        a = s.book(Appointment("lena", "Zoë Brändli", datetime.datetime(2026, 9, 8, 9, 0), "short"))
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "schedule.json")
            save(s, path)
            again = load(path)
        b = again.get(a.id)
        self.assertEqual((b.client, b.start, b.treatment), ("Zoë Brändli", a.start, "short"))


if __name__ == "__main__":
    unittest.main()
