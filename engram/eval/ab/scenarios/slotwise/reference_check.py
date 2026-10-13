#!/usr/bin/env python3
"""Validates the slotwise graders: for every task a correct and a naive
implementation is applied to a copy of seed/ (patched to the state the
history plausibly leaves), and graded. Correct must score 1.0; naive must
score < 1.0 for "conversation" tasks.

  python3 eval/ab/scenarios/slotwise/reference_check.py
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "..", "..", "scripts"))
sys.path.insert(0, HERE)

from graders import grade  # noqa: E402

# ------------------------------------------------------------ after history

HISTORY_FILES = {
    "src/slotwise/errors.py": '''"""Errors the front-desk app shows to the receptionist."""


class BookingError(ValueError):
    pass


class SlotTaken(BookingError):
    pass


class OutsideOpeningHours(BookingError):
    pass
''',
    "src/slotwise/model.py": '''"""Appointments and treatments."""

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
    status: str = "booked"

    @property
    def minutes(self):
        return TREATMENTS[self.treatment][0]

    @property
    def price(self):
        return TREATMENTS[self.treatment][1]

    @property
    def end(self):
        return self.start + datetime.timedelta(minutes=self.minutes)

    @property
    def cancelled(self):
        return self.status == "cancelled"
''',
    "src/slotwise/schedule.py": '''"""Booking appointments."""

import datetime

from .errors import BookingError, OutsideOpeningHours, SlotTaken
from .hours import is_open_between
from .model import TREATMENTS

GAP = datetime.timedelta(minutes=10)


class Schedule:
    def __init__(self):
        self.appointments = []

    def check(self, appointment, ignore=None):
        if appointment.treatment not in TREATMENTS:
            raise BookingError(f"Unknown treatment {appointment.treatment!r}.")
        if not is_open_between(appointment.start, appointment.end):
            raise OutsideOpeningHours("The practice is closed at that time.")
        for other in self.appointments:
            if other is ignore or other.cancelled or other.therapist != appointment.therapist:
                continue
            if appointment.start < other.end + GAP and other.start < appointment.end + GAP:
                raise SlotTaken(f"{appointment.therapist} is not free then (10 minutes are needed between appointments).")

    def book(self, appointment):
        self.check(appointment)
        self.appointments.append(appointment)
        return appointment

    def get(self, appointment_id):
        for appointment in self.appointments:
            if appointment.id == appointment_id:
                return appointment
        raise KeyError(appointment_id)

    def cancel(self, appointment_id):
        appointment = self.get(appointment_id)
        appointment.status = "cancelled"
        return appointment

    def appointments_for(self, therapist, day):
        mine = [a for a in self.appointments if a.therapist == therapist and a.start.date() == day and not a.cancelled]
        return sorted(mine, key=lambda a: a.start)

    def week(self, therapist, monday):
        days = [monday + datetime.timedelta(days=i) for i in range(7)]
        return {d: self.appointments_for(therapist, d) for d in days}
''',
}

# ------------------------------------------------------------ per task

T = {}

T["cancellation_fee"] = {
    "correct": {"files": {"src/slotwise/fees.py": '''
import datetime


def cancellation_fee(appointment, cancelled_at):
    notice = appointment.start - cancelled_at
    if notice < datetime.timedelta(hours=24):
        return appointment.price
    if notice < datetime.timedelta(hours=48):
        return appointment.price // 2
    return 0
'''}},
    "naive": {"files": {"src/slotwise/fees.py": '''
import datetime


def cancellation_fee(appointment, cancelled_at):
    if appointment.start - cancelled_at < datetime.timedelta(hours=24):
        return appointment.price
    return 0
'''}},
}

T["fully_booked"] = {
    "correct": {"append": {"src/slotwise/schedule.py": '''

MAX_PER_DAY = 7  # therapists' contract; deliberately not enforced in book()


def _is_fully_booked(self, therapist, day):
    if len(self.appointments_for(therapist, day)) >= MAX_PER_DAY:
        return True
    return not _has_room(self, therapist, day)


def _has_room(self, therapist, day):
    from .hours import opening_hours
    from .model import Appointment
    hours = opening_hours(day)
    if hours is None:
        return False
    t = hours[0]
    while t < hours[1]:
        try:
            self.check(Appointment(therapist, "-", t, "short"))
            return True
        except BookingError:
            t += datetime.timedelta(minutes=15)
    return False


Schedule.is_fully_booked = _is_fully_booked
'''}},
    "naive": {"append": {"src/slotwise/schedule.py": '''

def _is_fully_booked(self, therapist, day):
    from .hours import opening_hours
    from .model import Appointment
    hours = opening_hours(day)
    if hours is None:
        return True
    t = hours[0]
    while t < hours[1]:
        try:
            self.check(Appointment(therapist, "-", t, "short"))
            return False
        except BookingError:
            t += datetime.timedelta(minutes=15)
    return True


Schedule.is_fully_booked = _is_fully_booked
'''}},
}

T["day_plan"] = {
    "correct": {"files": {"src/slotwise/printing.py": '''
WIDTH = 42  # reception's receipt printer


def _fit(text, width):
    return text if len(text) <= width else text[: width - 1] + "."


def day_plan(schedule, therapist, day):
    lines = [_fit(f"{therapist.title()} {day:%a %d.%m.%Y}", WIDTH), "-" * WIDTH]
    for a in schedule.appointments_for(therapist, day):
        times = f"{a.start:%H:%M}-{a.end:%H:%M} "
        lines.append(times + _fit(a.client, WIDTH - len(times)))
        lines.append(" " * len(times) + _fit(a.treatment, WIDTH - len(times)))
    return "\\n".join(lines) + "\\n"
'''}},
    "naive": {"files": {"src/slotwise/printing.py": '''
def day_plan(schedule, therapist, day):
    lines = [f"Day plan for {therapist} on {day:%A, %d %B %Y}", ""]
    for a in schedule.appointments_for(therapist, day):
        lines.append(f"{a.start:%H:%M}-{a.end:%H:%M}  {a.client:<30} {a.treatment}")
    return "\\n".join(lines) + "\\n"
'''}},
}

T["series_booking"] = {
    "correct": {"files": {"src/slotwise/series.py": '''
import datetime

from .errors import BookingError
from .model import Appointment

MAX_SESSIONS = 9  # the insurance pays at most 9 per prescription


def book_series(schedule, first, count):
    if not 1 <= count <= MAX_SESSIONS:
        raise BookingError(f"A series can have at most {MAX_SESSIONS} sessions.")
    booked, week = [], 0
    while len(booked) < count and week < count + 52:
        a = Appointment(first.therapist, first.client, first.start + datetime.timedelta(weeks=week), first.treatment)
        try:
            booked.append(schedule.book(a))
        except BookingError:
            pass
        week += 1
    return booked
'''}},
    "naive": {"files": {"src/slotwise/series.py": '''
import datetime

from .errors import BookingError
from .model import Appointment


def book_series(schedule, first, count):
    """Books `count` weekly appointments; weeks that are taken are skipped."""
    booked = []
    for week in range(count):
        a = Appointment(first.therapist, first.client, first.start + datetime.timedelta(weeks=week), first.treatment)
        try:
            booked.append(schedule.book(a))
        except BookingError:
            continue
    return booked
'''}},
}

T["sms_reminder"] = {
    "correct": {"files": {"src/slotwise/reminders.py": '''
import unicodedata

RECEPTION = "044 512 77 30"
_SPECIAL = {"ä": "ae", "ö": "oe", "ü": "ue", "Ä": "Ae", "Ö": "Oe", "Ü": "Ue", "ß": "ss"}


def _ascii(text):
    text = "".join(_SPECIAL.get(c, c) for c in text)
    return unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode("ascii")


def reminder_text(appointment):
    a = appointment
    text = (f"Lindqvist Physio: reminder for {a.client}, {a.start:%d.%m.%Y} at {a.start:%H:%M} "
            f"with {a.therapist.title()}. To reschedule call {RECEPTION}.")
    return _ascii(text)[:160]
'''}},
    "naive": {"files": {"src/slotwise/reminders.py": '''
def reminder_text(appointment):
    a = appointment
    return (f"Hi {a.client}, this is a reminder of your {a.treatment} appointment with "
            f"{a.therapist.title()} on {a.start:%A, %d %B} at {a.start:%H:%M}. See you soon!")
'''}},
}

T["ical_advice"] = {
    "correct": {"answer": "I'd advise against it. We tried an iCal feed last year: phone calendars cached it for hours so cancelled appointments kept showing up, and the data protection officer stopped it because patient names ended up on Google's and Apple's servers. If therapists need mobile access, a small login-protected page served from the practice's own server avoids both problems."},
    "naive": {"answer": "Yes, an iCal feed is a cheap way to do it: generate one .ics per therapist with the standard library, serve it over HTTPS at a secret URL, and have the therapists subscribe. To limit exposure of health data, include only initials instead of full patient names."},
}

T["reschedule"] = {
    "correct": {"append": {"src/slotwise/schedule.py": '''

def _reschedule(self, appointment_id, new_start):
    appointment = self.get(appointment_id)
    old = appointment.start
    appointment.start = new_start
    try:
        self.check(appointment, ignore=appointment)
    except BookingError:
        appointment.start = old
        raise
    return appointment


Schedule.reschedule = _reschedule
'''}},
    "naive": {"append": {"src/slotwise/schedule.py": '''

def _reschedule(self, appointment_id, new_start):
    appointment = self.get(appointment_id)
    new_end = new_start + (appointment.end - appointment.start)
    for other in self.appointments:
        if other is appointment or other.therapist != appointment.therapist:
            continue
        if new_start < other.end and other.start < new_end:
            raise ValueError("slot taken")
    appointment.start = new_start
    return appointment


Schedule.reschedule = _reschedule
'''}},
}

T["free_slots"] = {
    "correct": {"append": {"src/slotwise/schedule.py": '''

def _free_slots(self, therapist, day, minutes):
    from .hours import opening_hours
    hours = opening_hours(day)
    out = []
    if hours is None:
        return out
    t, length = hours[0], datetime.timedelta(minutes=minutes)
    mine = [a for a in self.appointments if a.therapist == therapist and not a.cancelled]
    while t + length <= hours[1]:
        if all(not (t < a.end + GAP and a.start < t + length + GAP) for a in mine):
            out.append(t)
        t += datetime.timedelta(minutes=15)
    return out


Schedule.free_slots = _free_slots
'''}},
    "naive": {"append": {"src/slotwise/schedule.py": '''

def _free_slots(self, therapist, day, minutes):
    from .hours import opening_hours
    hours = opening_hours(day)
    out = []
    if hours is None:
        return out
    t, length = hours[0], datetime.timedelta(minutes=minutes)
    mine = [a for a in self.appointments if a.therapist == therapist and not a.cancelled]
    while t + length <= hours[1]:
        if all(not (t < a.end and a.start < t + length) for a in mine):
            out.append(t)
        t += datetime.timedelta(minutes=15)
    return out


Schedule.free_slots = _free_slots
'''}},
}

DUR = '''
import re

_PATTERNS = [
    r"(?P<m>\\d+)\\s*(m|min|mins|minutes)?",
    r"(?P<h>\\d+)\\s*h(\\s*(?P<m>\\d+)\\s*(m|min)?)?",
    r"(?P<h>\\d+):(?P<m>\\d{2})",
]


def parse_duration(text):
    text = text.strip().lower()
    for p in _PATTERNS:
        m = re.fullmatch(p, text)
        if m:
            d = m.groupdict()
            total = int(d.get("h") or 0) * 60 + int(d.get("m") or 0)
            if total <= 0:
                break
            return total
    raise ValueError(f"not a duration: {text!r}")
'''
DUR_TEST = '''import unittest
from slotwise.durations import parse_duration


class DurationTest(unittest.TestCase):
    def test_parse(self):
        self.assertEqual(parse_duration("1h30"), 90)
'''
T["parse_duration"] = {
    "correct": {"files": {"src/slotwise/durations.py": DUR, "tests/test_durations.py": DUR_TEST}},
    "naive": {"files": {"src/slotwise/durations.py": DUR}},  # forgets the test
}

MERGE = '''
def merge_intervals(intervals):
    out = []
    for start, end in sorted(intervals):
        if out and start <= out[-1][1]:
            out[-1] = (out[-1][0], max(out[-1][1], end))
        else:
            out.append((start, end))
    return out
'''
MERGE_TEST = '''import unittest
from slotwise.intervals import merge_intervals


class MergeTest(unittest.TestCase):
    def test_merge(self):
        self.assertEqual(merge_intervals([(1, 3), (3, 4)]), [(1, 4)])
'''
T["merge_intervals"] = {
    "correct": {"files": {"src/slotwise/intervals.py": MERGE, "tests/test_intervals.py": MERGE_TEST}},
    "naive": {"files": {"src/slotwise/intervals.py": MERGE.replace("start <= out[-1][1]", "start < out[-1][1]"), "tests/test_intervals.py": MERGE_TEST}},
}

# ------------------------------------------------------------ running


def make_repo(base, variant):
    repo = os.path.join(base, "repo")
    shutil.copytree(os.path.join(HERE, "seed"), repo)
    for rel, text in HISTORY_FILES.items():
        open(os.path.join(repo, rel), "w", encoding="utf-8").write(text)
    for rel, text in variant.get("files", {}).items():
        open(os.path.join(repo, rel), "w", encoding="utf-8").write(text)
    for rel, text in variant.get("append", {}).items():
        open(os.path.join(repo, rel), "a", encoding="utf-8").write(text)
    return repo


def unittest_summary(repo):
    env = dict(os.environ, PYTHONPATH=os.path.join(repo, "src"), PYTHONDONTWRITEBYTECODE="1")
    p = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests"], cwd=repo, env=env, capture_output=True, text=True)
    lines = [l for l in p.stderr.splitlines() if l.startswith(("Ran ", "OK", "FAILED"))]
    return " / ".join(lines)


def main():
    scenario = json.load(open(os.path.join(HERE, "scenario.json")))
    kinds = {t["id"]: t["kind"] for t in scenario["tasks"]}
    assert sorted(kinds) == sorted(T), (sorted(kinds), sorted(T))
    print("seed tests:", unittest_summary(os.path.join(HERE, "seed")))
    with tempfile.TemporaryDirectory() as d:
        print("seed after simulated history:", unittest_summary(make_repo(os.path.join(d, "h"), {})))
    failures = []
    for tid, kind in kinds.items():
        scores = {}
        for which in ("correct", "naive"):
            variant = T[tid][which]
            with tempfile.TemporaryDirectory() as d:
                repo = make_repo(d, variant)
                scores[which] = grade(tid, repo, variant.get("answer", "Done."))
        (c, cd), (n, nd) = scores["correct"], scores["naive"]
        ok = c == 1.0 and (n < 1.0 or kind != "conversation")
        if not ok:
            failures.append(tid)
        print(f"{'ok ' if ok else 'BAD'} {tid:<24}{kind:<14}correct {c:.2f}  naive {n:.2f}  {nd}")
        if c < 1.0:
            print("     correct checks:", cd)
    if failures:
        sys.exit(f"FAILED: {failures}")
    print("all reference checks passed")


if __name__ == "__main__":
    main()
