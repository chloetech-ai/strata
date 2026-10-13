"""Invoices."""

from dataclasses import dataclass
import datetime


@dataclass
class Invoice:
    number: str
    clinic: str  # clinic code
    patient_id: str
    patient_name: str
    issued: datetime.date
    amount_cents: int
    description: str = ""
