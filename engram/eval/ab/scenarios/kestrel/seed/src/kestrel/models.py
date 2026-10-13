"""Clinics, therapists, patients and appointments."""

from dataclasses import dataclass
import datetime

ASSESSMENT = "assessment"
FOLLOWUP = "followup"
KINDS = (ASSESSMENT, FOLLOWUP)

BOOKED = "booked"
CANCELLED = "cancelled"
ATTENDED = "attended"
NO_SHOW = "no_show"
STATUSES = (BOOKED, CANCELLED, ATTENDED, NO_SHOW)


@dataclass
class Clinic:
    code: str
    name: str


CLINICS = {
    "NG": Clinic("NG", "Kestrel Physio Northgate"),
    "HB": Clinic("HB", "Kestrel Physio Harbour"),
    "WK": Clinic("WK", "Kestrel Physio Westwick"),
}


@dataclass
class Therapist:
    id: str
    name: str
    clinic: str  # clinic code, see CLINICS


@dataclass
class Patient:
    id: str  # "KP-" and six digits, e.g. "KP-004211"
    name: str
    phone: str  # E.164, e.g. "+64211234567"


@dataclass
class Appointment:
    id: str
    patient_id: str
    therapist_id: str
    clinic: str
    start: datetime.datetime  # timezone-aware
    kind: str  # ASSESSMENT or FOLLOWUP
    treatment: str = ""  # what it is for, e.g. "lower back pain"
    status: str = BOOKED

    def __post_init__(self):
        if self.kind not in KINDS:
            raise ValueError(f"unknown kind {self.kind!r}")
        if self.status not in STATUSES:
            raise ValueError(f"unknown status {self.status!r}")
        if self.start.tzinfo is None:
            raise ValueError("start must be timezone-aware")
