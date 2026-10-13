"""Booking appointments."""

from .hours import is_open_between
from .model import TREATMENTS


class Schedule:
    def __init__(self):
        self.appointments = []

    def book(self, appointment):
        """Adds `appointment` if the practice is open and the therapist is free."""
        if appointment.treatment not in TREATMENTS:
            raise ValueError(f"unknown treatment {appointment.treatment!r}")
        if not is_open_between(appointment.start, appointment.end):
            raise ValueError("the practice is closed at that time")
        for other in self.appointments:
            if other.therapist != appointment.therapist:
                continue
            if other.start <= appointment.start < other.end:
                raise ValueError(f"{appointment.therapist} already has an appointment at that time")
        self.appointments.append(appointment)
        return appointment

    def get(self, appointment_id):
        for appointment in self.appointments:
            if appointment.id == appointment_id:
                return appointment
        raise KeyError(appointment_id)

    def appointments_for(self, therapist, day):
        """`therapist`'s appointments on `day`, by start time."""
        mine = [a for a in self.appointments if a.therapist == therapist and a.start.date() == day]
        return sorted(mine, key=lambda a: a.start)
