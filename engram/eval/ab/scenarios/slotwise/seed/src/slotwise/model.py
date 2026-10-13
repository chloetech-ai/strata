"""Appointments and treatments."""

from dataclasses import dataclass, field
import datetime
import itertools

# treatment -> (minutes, price in cents)
TREATMENTS = {
    "initial": (60, 14500),
    "standard": (45, 9800),
    "short": (30, 6800),
}

_ids = itertools.count(1)


@dataclass
class Appointment:
    therapist: str
    client: str
    start: datetime.datetime
    treatment: str = "standard"
    id: int = field(default_factory=lambda: next(_ids))

    @property
    def minutes(self):
        return TREATMENTS[self.treatment][0]

    @property
    def price(self):
        """Price in cents."""
        return TREATMENTS[self.treatment][1]

    @property
    def end(self):
        return self.start + datetime.timedelta(minutes=self.minutes)
