"""JSON storage for patients, therapists and appointments."""

import datetime
import json

from .models import Appointment, Patient, Therapist


def _appointment_to_dict(a):
    return {
        "id": a.id, "patient_id": a.patient_id, "therapist_id": a.therapist_id, "clinic": a.clinic,
        "start": a.start.isoformat(), "kind": a.kind, "treatment": a.treatment, "status": a.status,
    }


def _appointment_from_dict(d):
    return Appointment(
        id=d["id"], patient_id=d["patient_id"], therapist_id=d["therapist_id"], clinic=d["clinic"],
        start=datetime.datetime.fromisoformat(d["start"]), kind=d["kind"], treatment=d.get("treatment", ""),
        status=d.get("status", "booked"),
    )


def save(path, patients, therapists, appointments):
    data = {
        "patients": [vars(p) for p in patients],
        "therapists": [vars(t) for t in therapists],
        "appointments": [_appointment_to_dict(a) for a in appointments],
    }
    with open(path, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2, sort_keys=True)


def load(path):
    with open(path, encoding="utf-8") as f:
        data = json.load(f)
    patients = [Patient(**p) for p in data.get("patients", [])]
    therapists = [Therapist(**t) for t in data.get("therapists", [])]
    appointments = [_appointment_from_dict(d) for d in data.get("appointments", [])]
    return patients, therapists, appointments
