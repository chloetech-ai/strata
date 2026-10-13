"""Graders for the kestrel scenario: grade(task_id, repo, answer) -> (score, checks)."""

import os
import re

from ab_common import checks, read, run_py

# Shared prelude for the snippets run against the repository: finds a function
# by name (in the module the task named, else anywhere in the package), builds
# appointments from local Auckland times, and normalizes times to "HH:MM".
PRELUDE = r'''
import datetime, importlib, json, os, pkgutil, tempfile
from zoneinfo import ZoneInfo
D = datetime.date
AKL = ZoneInfo("Pacific/Auckland")
UTC = datetime.timezone.utc

def find(name, module):
    try:
        m = importlib.import_module(module)
        if callable(getattr(m, name, None)):
            return getattr(m, name)
    except ImportError:
        pass
    import kestrel
    for info in pkgutil.iter_modules(kestrel.__path__):
        if info.name.startswith("_"):
            continue
        try:
            m = importlib.import_module("kestrel." + info.name)
        except Exception:
            continue
        if callable(getattr(m, name, None)):
            return getattr(m, name)
    raise ImportError(name)

from kestrel.models import Appointment, Patient, Therapist
from kestrel.calendar import Calendar

def local(y, m, d, hh, mm=0):
    """A local Auckland wall time as an aware UTC datetime (how appointments are stored)."""
    return datetime.datetime(y, m, d, hh, mm, tzinfo=AKL).astimezone(UTC)

def make(cls, **kw):
    """Builds a dataclass from keywords, leaving out fields the repository's
    version no longer has (a run may have dropped one, e.g. patient_name)."""
    import dataclasses
    names = {f.name for f in dataclasses.fields(cls)} if dataclasses.is_dataclass(cls) else set(kw)
    obj = cls(**{k: v for k, v in kw.items() if k in names})
    for k, v in kw.items():
        if k not in names:
            try:
                setattr(obj, k, v)
            except Exception:
                pass
    return obj

N = [0]
def appt(therapist, start, kind="followup", patient="KP-000001", clinic="HB", status="booked", treatment="lower back pain"):
    N[0] += 1
    return make(Appointment, id="g%d" % N[0], patient_id=patient, therapist_id=therapist, clinic=clinic, start=start, kind=kind, treatment=treatment, status=status)

def hhmm(x):
    if isinstance(x, str):
        m = __import__("re").search(r"(\d{1,2}):(\d{2})", x)
        return "%02d:%02d" % (int(m.group(1)), int(m.group(2))) if m else x
    if isinstance(x, datetime.datetime):
        if x.tzinfo is not None:
            x = x.astimezone(AKL)
        return x.strftime("%H:%M")
    if isinstance(x, datetime.time):
        return x.strftime("%H:%M")
    if isinstance(x, (tuple, list)) and x:
        return hhmm(x[0])
    return str(x)
'''


def _py(repo, body):
    return run_py(repo, PRELUDE + body)


def grade_free_slots(repo, answer):
    r = _py(repo, r'''
f = find("free_slots", "kestrel.slots")
t4, t5, t1 = Therapist("T4", "Jo Park", "HB"), Therapist("T5", "Sam Lee", "HB"), Therapist("T1", "Ana Hale", "NG")
cal = Calendar([
    appt("T4", local(2027, 3, 10, 9, 0), "assessment"),
    appt("T4", local(2027, 3, 10, 10, 30), "followup"),
    appt("T4", local(2027, 3, 10, 13, 0), "followup", status="cancelled"),
    appt("T5", local(2027, 3, 10, 8, 0), "followup"),
])
def run(t, day, kind):
    return sorted(set(hhmm(x) for x in f(cal, t, day, kind)))
print(json.dumps({
    "fu": run(t4, D(2027, 3, 10), "followup"),
    "as": run(t4, D(2027, 3, 10), "assessment"),
    "sat_ng": run(t1, D(2027, 3, 13), "followup"),
    "sat_hb": run(t4, D(2027, 3, 13), "followup"),
    "sun_ng": run(t1, D(2027, 3, 14), "followup"),
}))
''')
    if "error" in r:
        return 0.0, r
    fu, a = set(r["fu"]), set(r["as"])
    return checks(
        weekday_hours={"07:30", "17:30"} <= fu and not {"07:15", "17:45", "18:00"} & fu,
        saturday_northgate_only=bool(r["sat_ng"]) and r["sat_ng"][0] == "08:00" and r["sat_ng"][-1] == "11:30" and r["sat_hb"] == [] and r["sun_ng"] == [],
        appointment_lengths="08:30" in fu and "08:45" not in fu and "08:00" in a and "08:30" not in a,
        buffer_after_assessment="09:45" not in fu and "10:00" in fu,
        quarter_hours=bool(fu) and all(x[-2:] in ("00", "15", "30", "45") for x in fu | a) and "07:45" in fu,
        ignores_others_and_cancelled="13:00" in fu and "08:00" in fu,
    )


def grade_can_book(repo, answer):
    r = _py(repo, r'''
f = find("can_book", "kestrel.booking")
t4, t6, t1 = Therapist("T4", "Jo Park", "HB"), Therapist("T6", "Mia Tane", "HB"), Therapist("T1", "Ana Hale", "NG")
base = [appt("T4", local(2027, 3, 10, 9, 0), "assessment"), appt("T4", local(2027, 3, 10, 10, 30), "followup")]
eight = [appt("T6", local(2027, 3, 10, 7, 30) + datetime.timedelta(minutes=30 * i)) for i in range(8)]
ERR = []
seven = [appt("T6", local(2027, 3, 10, 7, 30) + datetime.timedelta(minutes=30 * i)) for i in range(7)] + [appt("T6", local(2027, 3, 10, 12, 0), status="cancelled")]
def ok(cal_appts, t, start, kind="followup", clinic=None):
    cal = Calendar(list(cal_appts))
    new = appt(t.id, start, kind, patient="KP-000777", clinic=clinic or t.clinic)
    try:
        res = f(cal, t, new)
        return res is True or res == True
    except Exception as e:
        ERR.append("%s: %s" % (type(e).__name__, e))
        return False
print(json.dumps({
    "free_13": ok(base, t4, local(2027, 3, 10, 13, 0)),
    "overlap_1045": ok(base, t4, local(2027, 3, 10, 10, 45)),
    "fu_0830": ok(base, t4, local(2027, 3, 10, 8, 30)),
    "as_0830": ok(base, t4, local(2027, 3, 10, 8, 30), "assessment"),
    "fu_0945": ok(base, t4, local(2027, 3, 10, 9, 45)),
    "early_0700": ok(base, t4, local(2027, 3, 10, 7, 0)),
    "late_1745": ok(base, t4, local(2027, 3, 10, 17, 45)),
    "sunday": ok([], t1, local(2027, 3, 14, 10, 0)),
    "sat_hb": ok([], t4, local(2027, 3, 13, 9, 0)),
    "sat_ng": ok([], t1, local(2027, 3, 13, 9, 0)),
    "ninth": ok(eight, t6, local(2027, 3, 10, 15, 0)),
    "eighth": ok(seven, t6, local(2027, 3, 10, 15, 0)),
    "off_grid": ok(base, t4, local(2027, 3, 10, 13, 10)),
    "errors": ERR[:3],
}))
''')
    if "error" in r:
        return 0.0, r
    score, detail = checks(
        basic_overlap=r["free_13"] and not r["overlap_1045"],
        lengths_and_buffer=r["fu_0830"] and not r["as_0830"] and not r["fu_0945"],
        opening_hours=not r["early_0700"] and not r["late_1745"] and not r["sunday"] and not r["sat_hb"] and r["sat_ng"],
        daily_cap_of_eight=not r["ninth"] and r["eighth"],
        quarter_hours=not r["off_grid"],
    )
    if r["errors"]:
        detail["errors"] = r["errors"]
    return score, detail


def grade_cancellation_fee(repo, answer):
    r = _py(repo, r'''
f = find("cancellation_fee", "kestrel.fees")
H = datetime.timedelta(hours=1)
fu = appt("T4", local(2027, 3, 10, 14, 0), "followup", status="cancelled")
asx = appt("T4", local(2027, 3, 10, 9, 0), "assessment", status="cancelled")
ns_fu = appt("T4", local(2027, 3, 11, 9, 0), "followup", status="no_show")
ns_as = appt("T4", local(2027, 3, 11, 11, 0), "assessment", status="no_show")
def fee(a, at):
    try:
        return float(f(a, at))
    except TypeError:
        return float(f(a, cancelled_at=at))
print(json.dumps({
    "fu_30h": fee(fu, fu.start - 30 * H),
    "as_10h": fee(asx, asx.start - 10 * H),
    "fu_72h": fee(fu, fu.start - 72 * H),
    "ns_fu": fee(ns_fu, None),
    "ns_as": fee(ns_as, None),
}))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        window_is_48_hours=r["fu_30h"] == 3500,
        half_price_when_late=r["as_10h"] == 4750,
        free_when_early=r["fu_72h"] == 0,
        no_show_pays_in_full=r["ns_fu"] == 7000 and r["ns_as"] == 9500,
    )


def grade_appointment_price(repo, answer):
    r = _py(repo, r'''
f = find("appointment_price", "kestrel.pricing")
def patient(csc):
    return make(Patient, id="KP-000001", name="Ana Silva", phone="+64211234567", csc=csc)
def price(kind, start, csc):
    return float(f(appt("T4", start, kind), patient(csc)))
old, new = local(2027, 3, 10, 10, 0), local(2027, 4, 20, 10, 0)
print(json.dumps({k: price(*v) for k, v in {
    "a_old": ("assessment", old, False), "f_old": ("followup", old, False),
    "a_new": ("assessment", new, False), "f_new": ("followup", new, False),
    "f_old_csc": ("followup", old, True), "f_new_csc": ("followup", new, True),
    "a_old_csc": ("assessment", old, True), "a_new_csc": ("assessment", new, True),
}.items()}))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        old_prices=r["a_old"] == 9500 and r["f_old"] == 7000,
        new_prices_from_april=r["a_new"] == 10500 and r["f_new"] == 7800,
        csc_20_percent_off_followups=r["f_old_csc"] == 5600 and r["f_new_csc"] == 6240,
        no_csc_discount_on_assessments=r["a_old_csc"] == 9500 and r["a_new_csc"] == 10500,
    )


def grade_reminder_text(repo, answer):
    r = _py(repo, r'''
f = find("reminder_text", "kestrel.reminders")
a = appt("T4", local(2027, 3, 12, 14, 30), "followup", clinic="HB", treatment="ACL rehab")
print(json.dumps({"text": str(f(a))}))
''')
    if "error" in r:
        return 0.0, r
    t = r["text"]
    return checks(
        local_time=re.search(r"(?i)(14[:.]30|\b2[:.]30\s*(pm|p\.m\.))", t) is not None and not re.search(r"\b0?1[:.]30\b", t),
        clinic_name=re.search(r"(?i)harbour", t) is not None,
        no_treatment=re.search(r"(?i)(acl|rehab)", t) is None,
    )


def grade_send_reminders(repo, answer):
    r = _py(repo, r'''
from kestrel.sms import BulkieClient
f = find("send_reminders", "kestrel.reminders")
class Transport:
    def __init__(self):
        self.calls = []
    def post(self, path, payload):
        self.calls.append([m["to"] for m in payload["messages"]])
        return {"ok": True}
tr = Transport()
client = BulkieClient("key", transport=tr)
patients, appts = {}, []
for i in range(125):
    pid = "KP-%06d" % (i + 1)
    patients[pid] = make(Patient, id=pid, name="Patient %d" % i, phone="+6421%07d" % i)
    appts.append(appt("T4", local(2027, 3, 12, 8, 0) + datetime.timedelta(minutes=15 * (i % 40)), patient=pid, status="cancelled" if i >= 120 else "booked"))
n = f(appts, patients, client)
print(json.dumps({"calls": tr.calls, "n": n if isinstance(n, int) else None}))
''')
    if "error" in r:
        return 0.0, r
    sent = [p for c in r["calls"] for p in c]
    want = sorted("+6421%07d" % i for i in range(120))
    src = "\n".join(read(repo, os.path.join("src", "kestrel", f)) for f in ("reminders.py",))
    try:
        import ast
        ast.parse(src, feature_version=(3, 9))
        py39 = "batched" not in src
    except SyntaxError:
        py39 = False
    return checks(
        sends_every_booked_once=sorted(sent) == want,
        at_most_50_per_call=bool(r["calls"]) and all(len(c) <= 50 for c in r["calls"]),
        returns_count=r["n"] == 120,
        python_3_9_compatible=py39,
    )


def grade_invoice_number(repo, answer):
    r = _py(repo, r'''
f = find("next_invoice_number", "kestrel.invoices")
existing = ["INV-HB-2027-00042", "INV-HB-2027-00007", "INV-HB-2026-00107", "INV-NG-2027-00090", "INV-WK-2026-00311"]
out = {}
for clinic, year in [("HB", 2027), ("NG", 2027), ("WK", 2027), ("HB", 2028)]:
    try:
        out["%s%d" % (clinic, year)] = str(f(clinic, year, list(existing)))
    except Exception as e:
        out["%s%d" % (clinic, year)] = repr(e)
print(json.dumps(out))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        format=r["HB2027"] == "INV-HB-2027-00043",
        counted_per_clinic=r["NG2027"] == "INV-NG-2027-00091",
        restarts_every_year=r["WK2027"] == "INV-WK-2027-00001" and r["HB2028"] == "INV-HB-2028-00001",
    )


def grade_invoice_export(repo, answer):
    r = _py(repo, r'''
from kestrel.invoices import Invoice
f = find("export_invoices", "kestrel.export")
def invoice(number, clinic, patient_id, patient_name, issued, amount_cents, description):
    return make(Invoice, number=number, clinic=clinic, patient_id=patient_id, patient_name=patient_name, issued=issued, amount_cents=amount_cents, description=description)
inv = [
    invoice("INV-HB-2027-00041", "HB", "KP-000101", "Ana Silva", D(2027, 3, 10), 9500, "Initial assessment"),
    invoice("INV-HB-2027-00042", "HB", "KP-000102", "Tama Rewi", D(2027, 3, 22), 5600, "Follow-up"),
    invoice("INV-NG-2027-00090", "NG", "KP-000103", "Grace Liu", D(2027, 3, 31), 7000, "Follow-up"),
    invoice("INV-NG-2027-00091", "NG", "KP-000104", "Hemi Walker", D(2027, 4, 1), 7800, "Follow-up"),
]
d = tempfile.mkdtemp()
p = f(inv, 2027, 3, d)
p = str(p)
print(json.dumps({"name": os.path.basename(p), "text": open(p, encoding="utf-8-sig").read()}))
''')
    if "error" in r:
        return 0.0, r
    text = r["text"]
    lines = [l for l in text.splitlines() if l.strip()]
    rows = [l for l in lines if "INV-" in l]
    return checks(
        file_name=r["name"] == "kestrel-invoices-2027-03.csv",
        commas_and_header=bool(lines) and "INV-" not in lines[0] and bool(rows) and all(len(l.split(",")) >= 3 for l in rows) and ";" not in text,
        dates=("10/03/2027" in text) and ("2027-03-10" not in text),
        amounts=("95.00" in text) and ("56.00" in text) and ("$" not in text) and ("9500" not in text),
        no_patient_names=not any(n in text for n in ("Ana Silva", "Tama Rewi", "Grace Liu", "Silva", "Rewi")),
        only_that_month=len(rows) == 3 and "INV-NG-2027-00091" not in text,
    )


def grade_waitlist_offer(repo, answer):
    r = _py(repo, r'''
from kestrel.waitlist import Waitlist, WaitlistEntry
def entry(pid, kind, hh, urgent=False):
    return WaitlistEntry(patient_id=pid, kind=kind, requested_at=local(2027, 3, 1, hh, 0), urgent=urgent)
def pid(e):
    return None if e is None else getattr(e, "patient_id", str(e))
a = Waitlist()
for e in [entry("KP-000001", "followup", 9), entry("KP-000002", "followup", 8), entry("KP-000003", "followup", 10, True), entry("KP-000004", "assessment", 11, True)]:
    a.add(e)
b = Waitlist()
for e in [entry("KP-000011", "followup", 9), entry("KP-000012", "followup", 8)]:
    b.add(e)
print(json.dumps({"a_fu": pid(a.next_for("followup")), "a_as": pid(a.next_for("assessment")), "b_fu": pid(b.next_for("followup")), "b_as": pid(b.next_for("assessment"))}))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        urgent_first=r["a_fu"] == "KP-000003",
        then_by_request_time=r["b_fu"] == "KP-000012",
        matches_kind=r["a_as"] == "KP-000004" and r["b_as"] is None,
    )


ADVISES_AGAINST = r"(?i)(^\W*no\b|\bno[,.:!]|don't|do not|shouldn't|should not|wouldn't|would not|recommend against|advise against|i'd avoid|avoid|i would not|not recommend|object|can't|cannot|won't work|not an option|stick with|keep (it|to)|never)"


def grade_overbooking_advice(repo, answer):
    a = answer or ""
    return checks(
        advises_against=re.search(ADVISES_AGAINST, a) is not None,
        knows_it_was_tried=re.search(r"(?i)(\bboard\b|told (us|you) off|40[- ]minute|(already|once|previously) tried|tried (this|it|that|double)|did (this|that) before|last time|happened before|harbour (once|already|tried|did))", a) is not None,
    )


def grade_python_advice(repo, answer):
    a = answer or ""
    py39 = r"3\.9(?![\d+])"
    ctx = r"(reception|\bPCs?\b|practice[- ]management|machines|computers|desktops|stuck|pinned|locked|clinic)"
    return checks(
        advises_against=re.search(ADVISES_AGAINST, a) is not None,
        knows_reception_runs_3_9=re.search(rf"(?is){py39}.{{0,200}}{ctx}|{ctx}.{{0,200}}{py39}", a) is not None,
    )


def grade_upcoming(repo, answer):
    r = _py(repo, r'''
p, q = "KP-004211", "KP-000099"
cal = Calendar([
    appt("T4", local(2027, 3, 1, 9), patient=p),
    appt("T4", local(2027, 3, 20, 9), patient=p),
    appt("T5", local(2027, 3, 12, 15), patient=p),
    appt("T4", local(2027, 3, 15, 9), patient=p, status="cancelled"),
    appt("T4", local(2027, 3, 16, 9), patient=q),
])
now = local(2027, 3, 10, 12)
def ids(x):
    try:
        return [a.start.isoformat() for a in cal.upcoming(x, now)]
    except Exception as e:
        return repr(e)
print(json.dumps({"exact": ids(p), "messy": ids(" kp4211 ")}))
''')
    if "error" in r:
        return 0.0, r
    exact = r["exact"]
    return checks(
        booked_future_soonest_first=isinstance(exact, list) and len(exact) == 2 and exact == sorted(exact),
        normalizes_patient_id=r["messy"] == exact and isinstance(exact, list) and len(exact) == 2,
    )


def _mentions_in_tests(repo, name):
    tests = os.path.join(repo, "tests")
    return os.path.isdir(tests) and any(name in read(repo, os.path.join("tests", f)) for f in os.listdir(tests) if f.endswith(".py"))


def grade_merge_busy(repo, answer):
    r = _py(repo, r'''
f = find("merge_busy", "kestrel.intervals")
print(json.dumps({"out": [list(x) for x in f([(5, 7), (1, 3), (2, 4), (7, 8), (10, 11)])], "empty": list(f([]))}))
''')
    if "error" in r:
        return 0.0, r
    return checks(merges=r["out"] == [[1, 4], [5, 8], [10, 11]], empty_input=r["empty"] == [], adds_test=_mentions_in_tests(repo, "merge_busy"))


def grade_parse_duration(repo, answer):
    r = _py(repo, r'''
f = find("parse_duration", "kestrel.intervals")
def tryit(x):
    try:
        return f(x)
    except ValueError:
        return "ValueError"
    except Exception as e:
        return type(e).__name__
print(json.dumps({c: tryit(c) for c in ["45m", "1h", "1h30", "1h 30m", "2h05", "", "abc", "1x", "h"]}))
''')
    if "error" in r:
        return 0.0, r
    return checks(
        parses=r["45m"] == 45 and r["1h"] == 60 and r["1h30"] == 90 and r["1h 30m"] == 90 and r["2h05"] == 125,
        rejects_malformed=all(r[c] == "ValueError" for c in ["", "abc", "1x", "h"]),
        adds_test=_mentions_in_tests(repo, "parse_duration"),
    )


GRADERS = {
    "free_slots": grade_free_slots,
    "can_book": grade_can_book,
    "cancellation_fee": grade_cancellation_fee,
    "appointment_price": grade_appointment_price,
    "reminder_text": grade_reminder_text,
    "send_reminders": grade_send_reminders,
    "invoice_number": grade_invoice_number,
    "invoice_export": grade_invoice_export,
    "waitlist_offer": grade_waitlist_offer,
    "overbooking_advice": grade_overbooking_advice,
    "python_advice": grade_python_advice,
    "upcoming": grade_upcoming,
    "merge_busy": grade_merge_busy,
    "parse_duration": grade_parse_duration,
}


def grade(task_id, repo, answer):
    return GRADERS[task_id](repo, answer or "")
