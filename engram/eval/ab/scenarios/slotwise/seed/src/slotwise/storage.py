"""Saving and loading a schedule as JSON."""

import datetime
import json

from .model import Appointment
from .schedule import Schedule


def save(schedule, path):
    data = [
        {
            "id": a.id,
            "therapist": a.therapist,
            "client": a.client,
            "start": a.start.isoformat(timespec="minutes"),
            "treatment": a.treatment,
        }
        for a in schedule.appointments
    ]
    with open(path, "w", encoding="utf-8") as f:
        json.dump({"appointments": data}, f, indent=2, ensure_ascii=False)


def load(path):
    with open(path, encoding="utf-8") as f:
        data = json.load(f)
    schedule = Schedule()
    for d in data["appointments"]:
        start = datetime.datetime.fromisoformat(d["start"])
        schedule.appointments.append(Appointment(d["therapist"], d["client"], start, d["treatment"], d["id"]))
    return schedule
