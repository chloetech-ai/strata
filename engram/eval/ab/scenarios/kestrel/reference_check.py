#!/usr/bin/env python3
"""Validates the kestrel graders: for every task a correct and a naive
implementation is applied to a copy of seed/ (patched to the state the
history plausibly leaves), and graded. Correct must score 1.0; naive must
score < 1.0 for "conversation" tasks.

  python3 eval/ab/scenarios/kestrel/reference_check.py
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
    "src/kestrel/ids.py": '''"""Patient IDs."""

import re


def normalize_patient_id(text):
    m = re.fullmatch(r"KP-?(\\d{1,6})", text.strip().upper())
    if not m:
        raise ValueError(f"bad patient id {text!r}")
    return "KP-%06d" % int(m.group(1))
''',
    "src/kestrel/waitlist.py": '''"""The waitlist."""

from dataclasses import dataclass
import datetime


@dataclass
class WaitlistEntry:
    patient_id: str
    kind: str
    requested_at: datetime.datetime
    urgent: bool = False


class Waitlist:
    def __init__(self):
        self._entries = []

    def add(self, entry):
        self._entries.append(entry)

    def remove(self, patient_id):
        self._entries = [e for e in self._entries if e.patient_id != patient_id]

    def entries(self):
        return list(self._entries)
''',
    "src/kestrel/pricing.py": '''"""Session prices, in cents."""

from .models import ASSESSMENT

PRICES = {"assessment": 9500, "followup": 7000}


def session_price(kind):
    return PRICES[kind]
''',
}
HISTORY_APPEND = {
    "src/kestrel/calendar.py": '''

def _for_patient(self, patient_id):
    from .ids import normalize_patient_id
    pid = normalize_patient_id(patient_id)
    return [a for a in self.all() if a.patient_id == pid]


Calendar.for_patient = _for_patient
Calendar.all = lambda self: sorted(self._appointments.values(), key=lambda a: a.start)
''',
    "src/kestrel/models.py": "\nPatient.csc = False\n",
}

COMMON = '''
import datetime
from zoneinfo import ZoneInfo

AKL = ZoneInfo("Pacific/Auckland")
LEN = {"assessment": 45, "followup": 30}
'''

SLOT_CORE = COMMON + '''

def _hours(clinic, day):
    if day.weekday() < 5:
        return (7 * 60 + 30, 18 * 60)
    if day.weekday() == 5 and clinic == "NG":
        return (8 * 60, 12 * 60)
    return None


def _busy(calendar, therapist_id, day):
    out = []
    for a in calendar.all():
        if a.therapist_id != therapist_id or a.status != "booked":
            continue
        s = a.start.astimezone(AKL)
        if s.date() != day:
            continue
        m = s.hour * 60 + s.minute
        out.append((m, m + LEN[a.kind] + (15 if a.kind == "assessment" else 0)))
    return out


def _fits(m, kind, busy, hours, buffer_for_new=True):
    if hours is None or m % 15 or m < hours[0] or m + LEN[kind] > hours[1]:
        return False
    end = m + LEN[kind] + (15 if kind == "assessment" and buffer_for_new else 0)
    return all(end <= s or m >= e for s, e in busy)
'''

T = {}

T["free_slots"] = {
    "correct": {"files": {"src/kestrel/slots.py": SLOT_CORE + '''

def free_slots(calendar, therapist, day, kind):
    hours = _hours(therapist.clinic, day)
    if hours is None:
        return []
    busy = _busy(calendar, therapist.id, day)
    return [datetime.time(m // 60, m % 60) for m in range(hours[0], hours[1], 15) if _fits(m, kind, busy, hours)]
'''}},
    "naive": {"files": {"src/kestrel/slots.py": COMMON + '''

def free_slots(calendar, therapist, day, kind):
    taken = set()
    for a in calendar.all():
        if a.therapist_id == therapist.id and a.status == "booked":
            s = a.start.astimezone(AKL)
            if s.date() == day:
                taken.add(s.hour * 60 + s.minute)
    out = []
    for m in range(9 * 60, 17 * 60, 30):
        if all(not (t - 30 < m < t + 30) for t in taken):
            out.append(datetime.time(m // 60, m % 60))
    return out
'''}},
}

T["can_book"] = {
    "correct": {"files": {"src/kestrel/booking.py": SLOT_CORE + '''

def can_book(calendar, therapist, appointment):
    s = appointment.start.astimezone(AKL)
    day = s.date()
    busy = _busy(calendar, therapist.id, day)
    if len(busy) >= 8:
        return False
    return _fits(s.hour * 60 + s.minute, appointment.kind, busy, _hours(therapist.clinic, day))
'''}},
    "naive": {"files": {"src/kestrel/booking.py": COMMON + '''

def can_book(calendar, therapist, appointment):
    for a in calendar.all():
        if a.therapist_id == therapist.id and a.status == "booked":
            if abs((a.start - appointment.start).total_seconds()) < 30 * 60:
                return False
    return True
'''}},
}

T["cancellation_fee"] = {
    "correct": {"files": {"src/kestrel/fees.py": '''
import datetime
from .pricing import session_price


def cancellation_fee(appointment, cancelled_at=None):
    price = session_price(appointment.kind)
    if appointment.status == "no_show":
        return price
    if appointment.start - cancelled_at < datetime.timedelta(hours=48):
        return price // 2
    return 0
'''}},
    "naive": {"files": {"src/kestrel/fees.py": '''
import datetime
from .pricing import session_price


def cancellation_fee(appointment, cancelled_at=None):
    price = session_price(appointment.kind)
    if appointment.status == "no_show":
        return price
    if appointment.start - cancelled_at < datetime.timedelta(hours=24):
        return price // 2
    return 0
'''}},
}

T["appointment_price"] = {
    "correct": {"append": {"src/kestrel/pricing.py": '''

import datetime
from zoneinfo import ZoneInfo


def appointment_price(appointment, patient):
    new = appointment.start.astimezone(ZoneInfo("Pacific/Auckland")).date() >= datetime.date(2027, 4, 1)
    price = {"assessment": 10500, "followup": 7800}[appointment.kind] if new else session_price(appointment.kind)
    if appointment.kind == "followup" and getattr(patient, "csc", False):
        price = price * 80 // 100
    return price
'''}},
    "naive": {"append": {"src/kestrel/pricing.py": '''

def appointment_price(appointment, patient):
    return session_price(appointment.kind)
'''}},
}

T["reminder_text"] = {
    "correct": {"files": {"src/kestrel/reminders.py": COMMON + '''
from .models import CLINICS


def reminder_text(appointment):
    s = appointment.start.astimezone(AKL)
    return f"Hi! Reminder: you have an appointment at {CLINICS[appointment.clinic].name} on {s:%A %d %B} at {s:%H:%M}. See you then!"
'''}},
    "naive": {"files": {"src/kestrel/reminders.py": COMMON + '''
from .models import CLINICS


def reminder_text(appointment):
    s = appointment.start.astimezone(AKL)
    return f"Hi! Reminder: your {appointment.treatment} {appointment.kind} at {CLINICS[appointment.clinic].name} is on {s:%A %d %B} at {s:%H:%M}."
'''}},
}

T["send_reminders"] = {
    "correct": {"files": {"src/kestrel/reminders.py": '''

def send_reminders(appointments, patients, client):
    messages = [(patients[a.patient_id].phone, "Reminder: you have an appointment soon.") for a in appointments if a.status == "booked"]
    for i in range(0, len(messages), 50):
        client.send_batch(messages[i:i + 50])
    return len(messages)
'''}},
    "naive": {"files": {"src/kestrel/reminders.py": '''

def send_reminders(appointments, patients, client):
    messages = [(patients[a.patient_id].phone, "Reminder: you have an appointment soon.") for a in appointments if a.status == "booked"]
    client.send_batch(messages)
    return len(messages)
'''}},
}

T["invoice_number"] = {
    "correct": {"append": {"src/kestrel/invoices.py": '''

def next_invoice_number(clinic, year, existing):
    prefix = f"INV-{clinic}-{year}-"
    seqs = [int(n[len(prefix):]) for n in existing if n.startswith(prefix)]
    return f"{prefix}{max(seqs, default=0) + 1:05d}"
'''}},
    "naive": {"append": {"src/kestrel/invoices.py": '''

def next_invoice_number(clinic, year, existing):
    return f"{clinic}-{year}-{len(existing) + 1:04d}"
'''}},
}

T["invoice_export"] = {
    "correct": {"files": {"src/kestrel/export.py": '''
import csv
import os


def export_invoices(invoices, year, month, out_dir):
    path = os.path.join(out_dir, f"kestrel-invoices-{year:04d}-{month:02d}.csv")
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(["number", "date", "clinic", "patient_id", "description", "amount"])
        for i in invoices:
            if (i.issued.year, i.issued.month) == (year, month):
                w.writerow([i.number, i.issued.strftime("%d/%m/%Y"), i.clinic, i.patient_id, i.description, f"{i.amount_cents / 100:.2f}"])
    return path
'''}},
    "naive": {"files": {"src/kestrel/export.py": '''
import csv
import os


def export_invoices(invoices, year, month, out_dir):
    path = os.path.join(out_dir, f"invoices_{year}_{month}.csv")
    with open(path, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(["number", "date", "patient", "amount"])
        for i in invoices:
            if (i.issued.year, i.issued.month) == (year, month):
                w.writerow([i.number, i.issued.isoformat(), i.patient_name, f"${i.amount_cents / 100:.2f}"])
    return path
'''}},
}

T["waitlist_offer"] = {
    "correct": {"append": {"src/kestrel/waitlist.py": '''

def _next_for(self, kind):
    matches = [e for e in self._entries if e.kind == kind]
    return min(matches, key=lambda e: (not e.urgent, e.requested_at), default=None)


Waitlist.next_for = _next_for
'''}},
    "naive": {"append": {"src/kestrel/waitlist.py": '''

def _next_for(self, kind):
    matches = [e for e in self._entries if e.kind == kind]
    return min(matches, key=lambda e: e.requested_at, default=None)


Waitlist.next_for = _next_for
'''}},
}

T["overbooking_advice"] = {
    "correct": {"answer": "No. Harbour already tried double-booking the 07:30 slots, a patient waited 40 minutes and the physiotherapy board told us off. Use reminders and the no-show fee instead."},
    "naive": {"answer": "Yes, a modest overbooking of the first slot is a common way to cover no-shows; try it for a month and watch the wait times."},
}

T["python_advice"] = {
    "correct": {"answer": "I'd avoid it: match needs Python 3.10, and the reception PCs are stuck on 3.9 because of the practice-management software. A dict of handlers works there."},
    "naive": {"answer": "Go for it, match statements are a good fit for parsing and make the code much cleaner."},
}

T["upcoming"] = {
    "correct": {"append": {"src/kestrel/calendar.py": '''

def _upcoming(self, patient_id, now):
    return sorted((a for a in self.for_patient(patient_id) if a.status == "booked" and a.start > now), key=lambda a: a.start)


Calendar.upcoming = _upcoming
'''}},
    "naive": {"append": {"src/kestrel/calendar.py": '''

def _upcoming(self, patient_id, now):
    return sorted((a for a in self._appointments.values() if a.patient_id == patient_id and a.status == "booked" and a.start > now), key=lambda a: a.start)


Calendar.upcoming = _upcoming
'''}},
}

INTERVALS = '''
import re


def merge_busy(intervals):
    out = []
    for s, e in sorted(intervals):
        if out and s <= out[-1][1]:
            out[-1] = (out[-1][0], max(out[-1][1], e))
        else:
            out.append((s, e))
    return out


def parse_duration(text):
    m = re.fullmatch(r"\\s*(?:(\\d+)\\s*h)?\\s*(?:(\\d+)\\s*m?)?\\s*", text)
    if not m or not text.strip() or (m.group(1) is None and (m.group(2) is None or not text.strip().endswith("m"))):
        raise ValueError(text)
    return int(m.group(1) or 0) * 60 + int(m.group(2) or 0)
'''
INTERVAL_TEST = "# merge_busy parse_duration\n"

T["merge_busy"] = {
    "correct": {"files": {"src/kestrel/intervals.py": INTERVALS, "tests/test_intervals.py": INTERVAL_TEST}},
    "naive": {"files": {"src/kestrel/intervals.py": INTERVALS.replace("s <= out[-1][1]", "s < out[-1][1]"), "tests/test_intervals.py": INTERVAL_TEST}},
}
T["parse_duration"] = {
    "correct": {"files": {"src/kestrel/intervals.py": INTERVALS, "tests/test_intervals.py": INTERVAL_TEST}},
    "naive": {"files": {"src/kestrel/intervals.py": INTERVALS.replace("(\\\\d+)\\\\s*h", "(\\\\d+)h"), "tests/test_intervals.py": ""}},
}

# ------------------------------------------------------------ running


def make_repo(base, variant):
    repo = os.path.join(base, "repo")
    shutil.copytree(os.path.join(HERE, "seed"), repo)
    for rel, text in HISTORY_FILES.items():
        open(os.path.join(repo, rel), "w", encoding="utf-8").write(text)
    for rel, text in HISTORY_APPEND.items():
        open(os.path.join(repo, rel), "a", encoding="utf-8").write(text)
    for rel, text in variant.get("files", {}).items():
        mode = "a" if os.path.exists(os.path.join(repo, rel)) and rel.endswith("reminders.py") else "w"
        open(os.path.join(repo, rel), mode, encoding="utf-8").write(text)
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
