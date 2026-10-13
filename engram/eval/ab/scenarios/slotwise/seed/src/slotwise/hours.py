"""Opening hours of the practice."""

import datetime

_T = datetime.time

# weekday (Monday = 0) -> (opens, closes); days not listed are closed
OPENING_HOURS = {
    0: (_T(7, 30), _T(19, 0)),
    1: (_T(7, 30), _T(19, 0)),
    2: (_T(7, 30), _T(19, 0)),
    3: (_T(7, 30), _T(19, 0)),
    4: (_T(7, 30), _T(17, 0)),
    5: (_T(8, 0), _T(12, 0)),
}


def opening_hours(day):
    """(opens, closes) as datetimes on `day`, or None if the practice is closed."""
    hours = OPENING_HOURS.get(day.weekday())
    if hours is None:
        return None
    return datetime.datetime.combine(day, hours[0]), datetime.datetime.combine(day, hours[1])


def is_open_between(start, end):
    hours = opening_hours(start.date())
    return hours is not None and hours[0] <= start and end <= hours[1]
