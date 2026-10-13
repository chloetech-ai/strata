"""The appointment calendar."""

from .models import BOOKED


class Calendar:
    def __init__(self, appointments=()):
        self._appointments = {}
        for appointment in appointments:
            self.add(appointment)

    def add(self, appointment):
        if appointment.id in self._appointments:
            raise ValueError(f"duplicate appointment id {appointment.id}")
        self._appointments[appointment.id] = appointment
        return appointment

    def get(self, appointment_id):
        return self._appointments[appointment_id]

    def all(self):
        """Every appointment, in start order."""
        return sorted(self._appointments.values(), key=lambda a: a.id)

    def for_therapist(self, therapist_id):
        return [a for a in self.all() if a.therapist_id == therapist_id]

    def for_patient(self, patient_id):
        return [a for a in self.all() if a.patient_id == patient_id]

    def booked(self):
        return [a for a in self.all() if a.status == BOOKED]

    def __len__(self):
        return len(self._appointments)
