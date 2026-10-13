"""slotwise: appointment scheduling for a small physiotherapy practice."""

from .model import TREATMENTS, Appointment
from .schedule import Schedule

__all__ = ["TREATMENTS", "Appointment", "Schedule"]
