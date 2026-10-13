import os
import tempfile
import unittest

from helpers import appt, utc
from kestrel.models import Patient, Therapist
from kestrel.storage import load, save


class StorageTest(unittest.TestCase):
    def test_round_trip(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "kestrel.json")
            save(path, [Patient("KP-000001", "Ana Silva", "+64211234567")], [Therapist("T1", "Jo Park", "HB")],
                 [appt("a1", utc(2027, 3, 1, 20), treatment="ankle sprain")])
            patients, therapists, appointments = load(path)
        self.assertEqual(patients[0].name, "Ana Silva")
        self.assertEqual(therapists[0].clinic, "HB")
        self.assertEqual(appointments[0].start, utc(2027, 3, 1, 20))
        self.assertEqual(appointments[0].treatment, "ankle sprain")


if __name__ == "__main__":
    unittest.main()
