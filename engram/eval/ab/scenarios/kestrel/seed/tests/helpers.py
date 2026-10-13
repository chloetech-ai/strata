import datetime

from kestrel.models import Appointment

UTC = datetime.timezone.utc


def appt(id, start, kind="followup", patient="KP-000001", therapist="T1", clinic="HB", **kw):
    return Appointment(id=id, patient_id=patient, therapist_id=therapist, clinic=clinic, start=start, kind=kind, **kw)


def utc(y, m, d, hh=0, mm=0):
    return datetime.datetime(y, m, d, hh, mm, tzinfo=UTC)
