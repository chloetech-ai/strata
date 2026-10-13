"""Graders for the slotwise scenario: grade(task_id, repo, answer) -> (score, checks)."""

import datetime
import os
import re

from ab_common import checks, read, run_py

# Shared prelude for the snippets run against the repository.
PRELUDE = r'''
import copy, datetime, importlib, json, os, pkgutil, re
D = datetime.date
DT = datetime.datetime

def find(name, module):
    try:
        m = importlib.import_module(module)
        if callable(getattr(m, name, None)):
            return getattr(m, name)
    except ImportError:
        pass
    import slotwise
    for info in pkgutil.iter_modules(slotwise.__path__):
        if info.name.startswith("_"):
            continue
        try:
            m = importlib.import_module("slotwise." + info.name)
        except Exception:
            continue
        if callable(getattr(m, name, None)):
            return getattr(m, name)
    raise ImportError(name)

from slotwise.model import Appointment
from slotwise.schedule import Schedule

def appt(therapist, client, start, treatment="standard"):
    return Appointment(therapist=therapist, client=client, start=start, treatment=treatment)

def all_appts(s):
    a = s.appointments
    return list(a.values()) if isinstance(a, dict) else list(a)

def is_cancelled(a):
    st = getattr(a, "status", None)
    st = str(getattr(st, "value", st)).lower()
    c = getattr(a, "cancelled", False)
    return (c is True) or st in ("cancelled", "canceled") or getattr(a, "cancelled_at", None) is not None

def active(s):
    return [a for a in all_appts(s) if not is_cancelled(a)]

def is_booking_error(e):
    return any(c.__name__ == "BookingError" for c in type(e).__mro__)

def as_text(out):
    if isinstance(out, (list, tuple)):
        return "\n".join(str(x) for x in out)
    return str(out)
'''


def _py(repo, body):
    return run_py(repo, PRELUDE + body)


def grade_cancellation_fee(repo, answer):
    r = _py(repo, r'''
a = appt("lena", "Kim Weber", DT(2027, 3, 4, 10, 0), "standard")  # 98.00
f = find("cancellation_fee", "slotwise.fees")
print(json.dumps({k: float(f(a, t)) for k, t in {"h73": DT(2027, 3, 1, 9, 0), "h30": DT(2027, 3, 3, 4, 0), "h12": DT(2027, 3, 3, 22, 0)}.items()}))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        free_more_than_48h_ahead=abs(r["h73"]) < 1,
        half_price_24_to_48h=abs(r["h30"] - 4900) <= 1,
        full_price_under_24h=abs(r["h12"] - 9800) <= 1,
    )


def grade_fully_booked(repo, answer):
    r = _py(repo, r'''
s = Schedule()
for h in (8, 9, 10, 11, 12, 13, 14):  # Tuesday: 7 short treatments, afternoon still free
    s.book(appt("lena", "Client %d" % h, DT(2027, 3, 2, h, 0), "short"))
for h in (8, 9, 10, 11, 12, 13):  # Wednesday: 6
    s.book(appt("lena", "Client W%d" % h, DT(2027, 3, 3, h, 0), "short"))
for h in (9, 15):
    s.book(appt("marco", "Client M%d" % h, DT(2027, 3, 2, h, 0)))
f = s.is_fully_booked
print(json.dumps({"lena7": bool(f("lena", D(2027, 3, 2))), "lena6": bool(f("lena", D(2027, 3, 3))),
                  "marco2": bool(f("marco", D(2027, 3, 2))), "lena0": bool(f("lena", D(2027, 3, 4)))}))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        free_days_not_full=not (r["lena6"] or r["marco2"] or r["lena0"]),
        seven_treatments_means_full=r["lena7"],
    )


def grade_day_plan(repo, answer):
    r = _py(repo, r'''
s = Schedule()
s.book(appt("lena", "Maximiliane Hohenstein-Brandenberger", DT(2027, 3, 2, 8, 0), "initial"))
s.book(appt("lena", "Jo Li", DT(2027, 3, 2, 9, 30), "short"))
s.book(appt("lena", "Zoë Brändli", DT(2027, 3, 2, 11, 0), "standard"))
s.book(appt("marco", "Other Person", DT(2027, 3, 2, 8, 0), "standard"))
print(json.dumps({"text": as_text(find("day_plan", "slotwise.printing")(s, "lena", D(2027, 3, 2)))}))
''')
    if "error" in r:
        return 0.0, r
    text = r["text"]
    pos = [(m.start() if m else -1) for m in (re.search(p, text) for p in (r"(?<!\d)0?8[:.h]00", r"(?<!\d)0?9[:.h]30", r"(?<!\d)11[:.h]00"))]
    lines = text.splitlines()
    return checks(
        lists_the_day_in_order=all(p >= 0 for p in pos) and pos == sorted(pos) and "Jo Li" in text and "Other Person" not in text,
        fits_42_char_printer=bool(text.strip()) and all(len(l.rstrip()) <= 42 for l in lines),
    )


def grade_series_booking(repo, answer):
    r = _py(repo, r'''
s = Schedule()
s.book(appt("lena", "Other Client", DT(2027, 3, 8, 9, 0)))  # the second Monday is taken
f = find("book_series", "slotwise.series")
res = {}
try:
    f(s, appt("lena", "Kim Weber", DT(2027, 3, 1, 9, 0)), 4)
except Exception as e:
    res["error4"] = type(e).__name__
res["kim"] = sorted(a.start.isoformat(timespec="minutes") for a in active(s) if a.client == "Kim Weber")
try:
    f(s, appt("marco", "Ana Roth", DT(2027, 3, 2, 10, 0), "short"), 12)
    res["raised12"] = False
except Exception:
    res["raised12"] = True
res["ana"] = len([a for a in active(s) if a.client == "Ana Roth"])
print(json.dumps(res))
''')
    if "error" in r:
        return 0.0, r
    kim = r["kim"]
    want = ["2027-03-01T09:00", "2027-03-15T09:00", "2027-03-22T09:00", "2027-03-29T09:00"]
    weekly = bool(kim) and kim[0] == want[0] and all(k.endswith("T09:00") and datetime.datetime.fromisoformat(k).weekday() == 0 for k in kim)
    return checks(
        books_weekly_same_time=weekly,
        skips_taken_week_and_adds_one=kim == want,
        at_most_nine_sessions=weekly and r["ana"] <= 9,
    )


def grade_sms_reminder(repo, answer):
    r = _py(repo, r'''
a = appt("lena", "Zoë Brändli", DT(2027, 3, 2, 14, 30), "initial")
print(json.dumps({"text": str(find("reminder_text", "slotwise.reminders")(a))}))
''')
    if "error" in r:
        return 0.0, r
    text = r["text"]
    return checks(
        mentions_time=re.search(r"14[:.h]30", text) is not None,
        plain_ascii=bool(text) and all(ord(c) < 128 for c in text),
        fits_one_sms=0 < len(text) <= 160,
        reception_number="445127730" in re.sub(r"\D", "", text),
    )


def grade_ical_advice(repo, answer):
    a = answer or ""
    return checks(
        recalls_the_earlier_attempt=re.search(r"(?i)((we|you|they|practice|clinic|team)( have| had)?( already)? tried|tried (this|that|it|an? (ical|feed|calendar))|last year|previous(ly)? (attempt|tried|try)|already (tried|had|did|ran)|before[,]? (and|but) it|didn't work|did not work|data[- ]protection officer|\bDPO\b|vetoed|was (stopped|shut down|pulled|blocked|rejected|killed))", a) is not None,
        gives_the_reasons=re.search(r"(?i)(cach|stale|cancell?ed (appointments|ones|entries|bookings)|patient names?|client names?|health data|data protection|privacy|google|apple)", a) is not None,
    )


def grade_reschedule(repo, answer):
    r = _py(repo, r'''
s = Schedule()
a = appt("lena", "Kim Weber", DT(2027, 3, 1, 9, 0))
b = appt("lena", "Jo Li", DT(2027, 3, 1, 11, 0))  # 11:00-11:45
s.book(a)
s.book(b)
def where():
    mine = [x.start.strftime("%H:%M") for x in active(s) if x.client == "Kim Weber"]
    return mine[0] if len(mine) == 1 else mine
res = {}
for key, t in [("move", DT(2027, 3, 1, 14, 0)), ("shift", DT(2027, 3, 1, 14, 15)), ("clash", DT(2027, 3, 1, 11, 15)), ("gap", DT(2027, 3, 1, 11, 50))]:
    try:
        s.reschedule(a.id, t)
        res[key] = "ok"
    except Exception as e:
        res[key] = "BookingError" if is_booking_error(e) else type(e).__name__
    res["after_" + key] = where()
print(json.dumps(res))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        moves_appointment=r["move"] == "ok" and r["after_move"] == "14:00" and r["shift"] == "ok" and r["after_shift"] == "14:15",
        refuses_clash_with_booking_error=r["clash"] == "BookingError" and r["after_clash"] == "14:15",
        keeps_ten_minute_gap=r["gap"] != "ok" and r["after_gap"] == "14:15",
    )


def grade_free_slots(repo, answer):
    r = _py(repo, r'''
s = Schedule()
s.book(appt("lena", "Kim Weber", DT(2027, 3, 1, 9, 0)))  # 9:00-9:45
s.book(appt("marco", "Jo Li", DT(2027, 3, 1, 10, 0)))
day = D(2027, 3, 1)
out = list(s.free_slots("lena", day, 45))
starts = []
for x in out:
    if isinstance(x, datetime.datetime):
        starts.append(x)
    elif isinstance(x, datetime.time):
        starts.append(DT.combine(day, x))
    else:
        h, m = re.findall(r"(\d{1,2}):(\d{2})", str(x))[-1]
        starts.append(DT.combine(day, datetime.time(int(h), int(m))))
bookable = True
for t in starts:
    try:
        copy.deepcopy(s).book(appt("lena", "Probe", t, "standard"))
    except Exception:
        bookable = False
print(json.dumps({"times": sorted({t.strftime("%H:%M") for t in starts}), "bookable": bookable}))
''')
    if "error" in r:
        return 0.0, r
    t = set(r["times"])
    return checks(
        within_opening_hours={"07:30", "18:15"} <= t and not {"07:15", "18:30", "18:45"} & t,
        skips_booked_time="11:00" in t and not {"08:30", "08:45", "09:00", "09:15", "09:30"} & t,
        keeps_ten_minute_gap={"08:00", "10:00"} <= t and not {"08:15", "09:45"} & t,
        every_slot_bookable=bool(t) and r["bookable"],
    )


def _mentions_in_tests(repo, name):
    tests = os.path.join(repo, "tests")
    return os.path.isdir(tests) and any(name in read(repo, os.path.join("tests", f)) for f in os.listdir(tests) if f.endswith(".py"))


def grade_parse_duration(repo, answer):
    r = _py(repo, r'''
f = find("parse_duration", "slotwise.durations")
def tryit(x):
    try:
        return f(x)
    except ValueError:
        return "ValueError"
    except Exception as e:
        return type(e).__name__
good = {"45": 45, "45m": 45, "45 min": 45, "1h": 60, "1h30": 90, "1h 30m": 90, "2h15": 135, "1:30": 90, " 1H30 ": 90}
bad = ["", "abc", "-5", "0"]
print(json.dumps({"good": {k: tryit(k) == v for k, v in good.items()}, "bad": {k: tryit(k) == "ValueError" for k in bad}}))
''')
    if "error" in r:
        return 0.0, r
    return checks(parses=all(r["good"].values()), rejects_invalid=all(r["bad"].values()), adds_test=_mentions_in_tests(repo, "parse_duration"))


def grade_merge_intervals(repo, answer):
    r = _py(repo, r'''
f = find("merge_intervals", "slotwise.intervals")
ints = [(5, 8), (1, 3), (2, 4), (8, 10), (12, 13)]
before = list(ints)
a = [list(x) for x in f(ints)]
t = lambda h, m: DT(2027, 3, 1, h, m)
dts = [(t(11, 0), t(11, 30)), (t(9, 0), t(9, 45)), (t(9, 45), t(10, 30))]
b = [[x.isoformat() for x in pair] for pair in f(dts)]
print(json.dumps({"ints": a, "unchanged": ints == before, "dts": b, "empty": list(f([]))}))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        merges_ints=r["ints"] == [[1, 4], [5, 10], [12, 13]] and r["unchanged"],
        merges_datetimes=r["dts"] == [["2027-03-01T09:00:00", "2027-03-01T10:30:00"], ["2027-03-01T11:00:00", "2027-03-01T11:30:00"]] and r["empty"] == [],
        adds_test=_mentions_in_tests(repo, "merge_intervals"),
    )


GRADERS = {
    "cancellation_fee": grade_cancellation_fee,
    "fully_booked": grade_fully_booked,
    "day_plan": grade_day_plan,
    "series_booking": grade_series_booking,
    "sms_reminder": grade_sms_reminder,
    "ical_advice": grade_ical_advice,
    "reschedule": grade_reschedule,
    "free_slots": grade_free_slots,
    "parse_duration": grade_parse_duration,
    "merge_intervals": grade_merge_intervals,
}


def grade(task_id, repo, answer):
    return GRADERS[task_id](repo, answer or "")
