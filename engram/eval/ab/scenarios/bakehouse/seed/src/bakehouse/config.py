"""Bakery-wide settings."""

import datetime

BAKERY_NAME = "Larkspur Bakery"

# Orders for a delivery day must be in by this time on the day before.
ORDER_CUTOFF = datetime.time(14, 0)

# Weekdays on which we neither bake nor deliver (Monday is 0, Sunday is 6).
CLOSED_WEEKDAYS = (6,)
