#!/usr/bin/env python3
"""Convert LongMemEval (S + oracle) and LoCoMo to one common JSONL format and write STATS.md.

Input  (from download_memory_benchmarks.py):
  <root>/raw/longmemeval/longmemeval_s_cleaned.json      -> <root>/longmemeval_s.jsonl
  <root>/raw/longmemeval/longmemeval_oracle.json         -> <root>/longmemeval_oracle.jsonl
  <root>/raw/locomo/locomo10.json                        -> <root>/locomo.jsonl
  <root>/raw/longmemeval_original_deprecated/longmemeval_s.json  (optional; only used for a diff in STATS.md)
  <root>/raw/MANIFEST.json                               (download sizes / revisions for STATS.md)

One JSON object per line ("evaluation unit"):
{"dataset": "longmemeval_s" | "longmemeval_oracle" | "locomo",
 "unit_id": str,
 "turns": [{"turn_id": str, "session_id": str, "session_index": int, "turn_index": int,
            "role": "user" | "assistant" | <speaker name>, "text": str, "timestamp": str | null,
            # LoCoMo only, present only when the raw turn shared an image:
            "image_caption": str}],
 "questions": [{"qid": str, "question": str, "answer": str, "question_type": str,
                "question_date": str | null, "evidence_turn_ids": [str],
                "evidence_session_ids": [str], "abstention": bool,
                # LoCoMo only:
                "raw_category": int,
                # LoCoMo category 5 (adversarial) only:
                "adversarial_answer": str | null}]}

Conventions
  * session_index / turn_index are 0-based positions in the raw file order.
  * LongMemEval: turn_id = f"{session_id}:{turn_index}"; timestamp = the session's haystack date.
    A session id that occurs more than once in the same haystack (13 haystacks in S_cleaned; identical
    content, different dates) gets session_id f"{sid}#dup{k}" for its k-th repeat so turn ids stay unique.
    evidence_turn_ids = turns with has_answer == true inside answer sessions (any role);
    evidence_session_ids = answer_session_ids; abstention = question_id.endswith("_abs").
  * LoCoMo: turn_id = dia_id; session_id = "session_<N>"; timestamp = session_<N>_date_time.
    question_type = category name (code mapping, see CATEGORY_NAMES); abstention = category 5;
    for category 5 without a gold answer, answer = "Not mentioned in the conversation"
    (the option the official scorer rewards). Evidence ids are normalised (split "D8:6; D9:17",
    "D9:1 D4:4", fix "D:11:26" -> "D11:26", "D30:05" -> "D30:5"); unresolvable ids are dropped and
    reported in STATS.md.

Usage: python3 convert_memory_benchmarks.py [--root /home/user/data/memory]
"""
import argparse
import collections
import hashlib
import json
import os
import re
import sys
from datetime import datetime

# LoCoMo category id -> name. Source: the released LoCoMo code (snap-research/locomo,
# task_eval/evaluation.py: cat 1 scored with multi-answer F1 "multi-hop", cat 5 "adversarial";
# task_eval/gpt_utils.py: cat 2 gets the "Use DATE of CONVERSATION" hint -> temporal) and the
# question counts (4 = 841 direct factual questions -> single-hop; 3 = 96 "Would X likely..."
# inference questions -> open-domain). The LoCoMo paper's prose lists categories in a different
# order (single-hop, multi-hop, temporal, open-domain); most follow-up work uses this code mapping.
CATEGORY_NAMES = {1: "multi-hop", 2: "temporal", 3: "open-domain", 4: "single-hop", 5: "adversarial"}
ADVERSARIAL_GOLD = "Not mentioned in the conversation"


# ----------------------------------------------------------------------------- LongMemEval
def convert_longmemeval(path, dataset, report):
    data = json.load(open(path))
    units = []
    dup_sessions = []          # (qid, sid, count)
    missing_answer_sessions = []
    has_answer_outside = 0
    for e in data:
        qid = e["question_id"]
        sids, dates, sessions = e["haystack_session_ids"], e["haystack_dates"], e["haystack_sessions"]
        assert len(sids) == len(dates) == len(sessions), qid
        answer_ids = list(e["answer_session_ids"])
        answer_set = set(answer_ids)
        seen = collections.Counter()
        turns, ev_turns = [], []
        present = set()
        for si, (sid, date, sess) in enumerate(zip(sids, dates, sessions)):
            seen[sid] += 1
            out_sid = sid if seen[sid] == 1 else f"{sid}#dup{seen[sid] - 1}"
            if seen[sid] == 2:
                dup_sessions.append((qid, sid))
            present.add(sid)
            for ti, t in enumerate(sess):
                tid = f"{out_sid}:{ti}"
                turns.append({"turn_id": tid, "session_id": out_sid, "session_index": si, "turn_index": ti,
                              "role": t["role"], "text": t["content"], "timestamp": date})
                if t.get("has_answer") is True:
                    if sid in answer_set:
                        ev_turns.append(tid)
                    else:
                        has_answer_outside += 1
        for a in answer_ids:
            if a not in present:
                missing_answer_sessions.append((qid, a))
        q = {"qid": qid, "question": e["question"], "answer": str(e["answer"]),
             "question_type": e["question_type"], "question_date": e.get("question_date"),
             "evidence_turn_ids": ev_turns, "evidence_session_ids": answer_ids,
             "abstention": qid.endswith("_abs")}
        units.append({"dataset": dataset, "unit_id": qid, "turns": turns, "questions": [q]})
    report["dup_sessions"] = dup_sessions
    report["missing_answer_sessions"] = missing_answer_sessions
    report["has_answer_outside_answer_sessions"] = has_answer_outside
    return data, units


def iter_json_array(path, chunk=1 << 24):
    """The elements of a top-level JSON array, read a piece at a time (the
    LongMemEval_M file is 2.7 GB, too big to load whole)."""
    dec = json.JSONDecoder()
    buf = ""
    with open(path, encoding="utf-8") as f:
        started = False
        while True:
            more = f.read(chunk)
            buf += more
            while True:
                i = 0
                while i < len(buf) and buf[i] in " \t\r\n,":
                    i += 1
                if not started and i < len(buf) and buf[i] == "[":
                    started = True
                    buf = buf[i + 1:]
                    continue
                buf = buf[i:]
                if not buf or buf[0] == "]":
                    break
                try:
                    obj, end = dec.raw_decode(buf)
                except json.JSONDecodeError:
                    break
                yield obj
                buf = buf[end:]
            if not more:
                return


def convert_longmemeval_m(path, out, every):
    """Every `every`-th LongMemEval_M question, converted as LongMemEval_S is."""
    n = 0
    with open(out, "w") as f:
        for i, e in enumerate(iter_json_array(path)):
            if i % every:
                continue
            rep = {}
            tmp = os.path.join(os.path.dirname(out), ".lme_m_one.json")
            json.dump([e], open(tmp, "w"))
            _, units = convert_longmemeval(tmp, "longmemeval_m", rep)
            os.remove(tmp)
            f.write(json.dumps(units[0]) + "\n")
            n += 1
    return n


# ----------------------------------------------------------------------------- LoCoMo
DIA_RE = re.compile(r"^D:?(\d+):0*(\d+)$")


def normalise_evidence(raw_list, dia_ids):
    """Return (matched ids in order, list of (raw, token, status))."""
    out, log = [], []
    for raw in raw_list:
        raw_s = str(raw).strip()
        tokens = [x for x in re.split(r"[;,\s]+", raw_s) if x]
        for tok in tokens:
            if tok in dia_ids:
                cand, status = tok, ("ok" if tok == raw_s else "split")
            else:
                m = DIA_RE.match(tok)
                cand = f"D{int(m.group(1))}:{int(m.group(2))}" if m else None
                if cand in dia_ids:
                    status = "fixed"
                else:
                    cand, status = None, "unmatched"
            if status != "ok":
                log.append((raw_s, tok, status, cand))
            if cand and cand not in out:
                out.append(cand)
        if not tokens:
            log.append((raw_s, "", "unmatched", None))
    return out, log


def convert_locomo(path, report):
    data = json.load(open(path))
    units = []
    ev_log = []
    orphan_dates = []
    dup_ev = 0
    for s in data:
        sid_ = s["sample_id"]
        conv = s["conversation"]
        nums = sorted(int(k.split("_")[1]) for k in conv if re.fullmatch(r"session_\d+", k))
        for k in conv:
            m = re.fullmatch(r"session_(\d+)_date_time", k)
            if m and f"session_{m.group(1)}" not in conv:
                orphan_dates.append((sid_, k))
        turns, dia_to_sess = [], {}
        sess_index = {}
        for si, n in enumerate(nums):
            skey = f"session_{n}"
            sess_index[skey] = si
            ts = conv.get(f"{skey}_date_time")
            for ti, t in enumerate(conv[skey]):
                did = t["dia_id"]
                assert did not in dia_to_sess, (sid_, did)
                dia_to_sess[did] = skey
                rec = {"turn_id": did, "session_id": skey, "session_index": si, "turn_index": ti,
                       "role": t["speaker"], "text": t["text"], "timestamp": ts}
                if t.get("blip_caption"):
                    rec["image_caption"] = t["blip_caption"]
                turns.append(rec)
        questions = []
        for qi, qa in enumerate(s["qa"]):
            cat = qa["category"]
            if len(qa["evidence"]) != len(set(qa["evidence"])):
                dup_ev += 1
            ev, log = normalise_evidence(qa["evidence"], dia_to_sess)
            for item in log:
                ev_log.append((sid_, f"{sid_}:q{qi}", cat) + item)
            ev_sess = sorted({dia_to_sess[d] for d in ev}, key=lambda k: sess_index[k])
            if "answer" in qa:
                ans = str(qa["answer"])
            elif cat == 5:
                ans = ADVERSARIAL_GOLD
            else:
                raise ValueError(f"no answer for non-adversarial question {sid_}:q{qi}")
            q = {"qid": f"{sid_}:q{qi}", "question": qa["question"], "answer": ans,
                 "question_type": CATEGORY_NAMES[cat], "question_date": None,
                 "evidence_turn_ids": ev, "evidence_session_ids": ev_sess,
                 "abstention": cat == 5, "raw_category": cat}
            if cat == 5:
                q["adversarial_answer"] = qa.get("adversarial_answer")
            questions.append(q)
        units.append({"dataset": "locomo", "unit_id": sid_, "turns": turns, "questions": questions})
    report["evidence_log"] = ev_log
    report["orphan_dates"] = orphan_dates
    report["dup_evidence_lists"] = dup_ev
    return data, units


# ----------------------------------------------------------------------------- IO + checks
def write_jsonl(units, path):
    tmp = path + ".part"
    with open(tmp, "w", encoding="utf-8") as f:
        for u in units:
            f.write(json.dumps(u, ensure_ascii=False) + "\n")
    os.replace(tmp, path)
    return os.path.getsize(path)


def validate(path):
    """Re-read a JSONL file and check the schema + referential integrity. Returns #units."""
    n = 0
    tkeys = {"turn_id", "session_id", "session_index", "turn_index", "role", "text", "timestamp"}
    qkeys = {"qid", "question", "answer", "question_type", "question_date", "evidence_turn_ids",
             "evidence_session_ids", "abstention"}
    qids = set()
    with open(path, encoding="utf-8") as f:
        for line in f:
            u = json.loads(line)
            n += 1
            assert set(u) == {"dataset", "unit_id", "turns", "questions"}, u.keys()
            assert isinstance(u["unit_id"], str)
            tids = [t["turn_id"] for t in u["turns"]]
            assert len(tids) == len(set(tids)), f"dup turn ids in {u['unit_id']}"
            sids = set()
            for t in u["turns"]:
                assert tkeys <= set(t) and set(t) - tkeys <= {"image_caption"}, t.keys()
                assert isinstance(t["text"], str) and isinstance(t["role"], str)
                assert isinstance(t["session_index"], int) and isinstance(t["turn_index"], int)
                assert t["timestamp"] is None or isinstance(t["timestamp"], str)
                sids.add(t["session_id"])
            tidset = set(tids)
            for q in u["questions"]:
                assert qkeys <= set(q) and set(q) - qkeys <= {"raw_category", "adversarial_answer"}, q.keys()
                assert q["qid"] not in qids, q["qid"]
                qids.add(q["qid"])
                assert isinstance(q["answer"], str) and isinstance(q["abstention"], bool)
                for tid in q["evidence_turn_ids"]:
                    assert tid in tidset, (u["unit_id"], tid)
                for s in q["evidence_session_ids"]:
                    assert s in sids, (u["unit_id"], s)
    return n


# ----------------------------------------------------------------------------- stats
def unit_stats(units):
    qtype = collections.Counter()
    qtype_abs = collections.Counter()
    qtype_empty = collections.Counter()
    n_q = n_turns = 0
    chars = []
    sessions = 0
    empty_ev_turn = empty_ev_sess = empty_ev_turn_nonabs = 0
    empty_text_turns = 0
    for u in units:
        n_turns += len(u["turns"])
        chars.append(sum(len(t["text"]) for t in u["turns"]))
        sessions += len({t["session_id"] for t in u["turns"]})
        empty_text_turns += sum(1 for t in u["turns"] if not t["text"].strip())
        for q in u["questions"]:
            n_q += 1
            qtype[q["question_type"]] += 1
            if q["abstention"]:
                qtype_abs[q["question_type"]] += 1
            if not q["evidence_turn_ids"]:
                empty_ev_turn += 1
                qtype_empty[q["question_type"]] += 1
                if not q["abstention"]:
                    empty_ev_turn_nonabs += 1
            if not q["evidence_session_ids"]:
                empty_ev_sess += 1
    total = sum(chars)
    return {"units": len(units), "questions": n_q, "turns": n_turns, "sessions": sessions,
            "chars_total": total, "chars_mean": total / len(units), "chars_min": min(chars),
            "chars_max": max(chars), "qtype": qtype, "qtype_abs": qtype_abs, "qtype_empty": qtype_empty,
            "empty_ev_turn": empty_ev_turn, "empty_ev_turn_nonabs": empty_ev_turn_nonabs,
            "empty_ev_sess": empty_ev_sess, "abstention": sum(qtype_abs.values()),
            "empty_text_turns": empty_text_turns}


def session_hash(sess):
    return hashlib.sha256(json.dumps([[t["role"], t["content"]] for t in sess],
                                     ensure_ascii=False).encode()).hexdigest()


def lme_session_dedup(raw):
    occ = 0
    by_id = {}
    by_hash = set()
    id_hashes = collections.defaultdict(set)
    chars_unique_hash = {}
    for e in raw:
        for sid, sess in zip(e["haystack_session_ids"], e["haystack_sessions"]):
            occ += 1
            h = session_hash(sess)
            by_hash.add(h)
            id_hashes[sid].add(h)
            by_id[sid] = True
            chars_unique_hash[h] = sum(len(t["content"]) for t in sess)
    ids_multi_content = sum(1 for v in id_hashes.values() if len(v) > 1)
    hash_ids = collections.defaultdict(set)
    for sid, hs in id_hashes.items():
        for h in hs:
            hash_ids[h].add(sid)
    hashes_multi_id = sum(1 for v in hash_ids.values() if len(v) > 1)
    hashes_answer_and_filler = sum(1 for v in hash_ids.values()
                                   if any(x.startswith("answer_") for x in v)
                                   and any(not x.startswith("answer_") for x in v))
    answer_ids = {sid for sid in by_id if sid.startswith("answer_")}
    return {"occurrences": occ, "distinct_ids": len(by_id), "distinct_hashes": len(by_hash),
            "ids_with_multiple_contents": ids_multi_content, "contents_with_multiple_ids": hashes_multi_id,
            "contents_answer_and_filler": hashes_answer_and_filler,
            "distinct_answer_ids": len(answer_ids),
            "distinct_non_answer_ids": len(by_id) - len(answer_ids),
            "unique_content_chars": sum(chars_unique_hash.values())}


def _pd(s):
    # "2023/05/20 (Sat) 02:32"
    day, rest = s.split(" (")
    return datetime.strptime(day + " " + rest.split(") ")[1], "%Y/%m/%d %H:%M")


def lme_extra(raw):
    """Ordering / evidence-role diagnostics for a raw LongMemEval file."""
    inv_any = inv_day = q_before_last = q_before_last_sameday = 0
    ev_roles = collections.Counter()
    q_ev_only_assistant = 0
    ans_sess_no_turn = 0
    q_some_ans_sess_no_turn = 0
    for e in raw:
        ds = [_pd(x) for x in e["haystack_dates"]]
        if ds != sorted(ds):
            inv_any += 1
        if [d.date() for d in ds] != sorted(d.date() for d in ds):
            inv_day += 1
        qd = _pd(e["question_date"])
        if qd < max(ds):
            q_before_last += 1
            if qd.date() == max(ds).date():
                q_before_last_sameday += 1
        roles = collections.Counter()
        aset = set(e["answer_session_ids"])
        flag = False
        for sid, sess in zip(e["haystack_session_ids"], e["haystack_sessions"]):
            if sid in aset:
                if not any(t.get("has_answer") for t in sess):
                    ans_sess_no_turn += 1
                    flag = True
                for t in sess:
                    if t.get("has_answer"):
                        roles[t["role"]] += 1
        ev_roles.update(roles)
        if roles and not roles.get("user"):
            q_ev_only_assistant += 1
        q_some_ans_sess_no_turn += flag
    return {"haystacks_with_time_inversions": inv_any, "haystacks_with_day_inversions": inv_day,
            "question_before_last_session": q_before_last,
            "question_before_last_session_same_day": q_before_last_sameday,
            "evidence_turn_roles": ev_roles, "questions_evidence_only_assistant": q_ev_only_assistant,
            "answer_sessions_without_has_answer_turn": ans_sess_no_turn,
            "questions_with_such_answer_session": q_some_ans_sess_no_turn}


def lme_diff(clean, orig):
    oc = {e["question_id"]: e for e in orig}
    changed_fields = collections.Counter()
    removed = collections.Counter()
    removed_total = removed_empty = added_total = 0
    for e in clean:
        o = oc[e["question_id"]]
        for f in ["question_type", "question", "answer", "question_date", "answer_session_ids"]:
            if e[f] != o[f]:
                changed_fields[f] += 1
        ce = collections.Counter(e["haystack_session_ids"])
        co = collections.Counter(o["haystack_session_ids"])
        rem = co - ce
        add = ce - co
        removed[sum(rem.values())] += 1
        removed_total += sum(rem.values())
        added_total += sum(add.values())
        osess = collections.defaultdict(list)
        for sid, sess in zip(o["haystack_session_ids"], o["haystack_sessions"]):
            osess[sid].append(sess)
        for sid, k in rem.items():
            removed_empty += sum(1 for s in osess[sid][-k:] if not s)
    return {"questions": len(clean), "changed_fields": changed_fields,
            "questions_with_removed_sessions": sum(v for k, v in removed.items() if k > 0),
            "removed_per_question": dict(sorted(removed.items())), "removed_total": removed_total,
            "removed_empty": removed_empty, "added_total": added_total,
            "orig_turns": sum(len(s) for e in orig for s in e["haystack_sessions"]),
            "clean_turns": sum(len(s) for e in clean for s in e["haystack_sessions"])}


def fmt_int(x):
    return f"{x:,}"


def write_stats(root, stats, manifest, out_sizes, extras):
    L = []
    L.append("# Memory benchmark conversion stats\n")
    L.append("Generated by `engram/scripts/convert_memory_benchmarks.py` (downloads: "
             "`engram/scripts/download_memory_benchmarks.py`). Character counts are over turn "
             "`text` only (LoCoMo image captions excluded); tokens ~= chars / 4.\n")
    L.append("## Sources and conventions\n")
    L.append("- `longmemeval_s.jsonl` <- `raw/longmemeval/longmemeval_s_cleaned.json` (HF `xiaowu0162/longmemeval-cleaned`, "
             "the 2025-09 cleaned release that the LongMemEval README now names as the official data; the original "
             "`xiaowu0162/longmemeval` repo is marked deprecated). The original S file is kept in "
             "`raw/longmemeval_original_deprecated/` and diffed below.")
    L.append("- `longmemeval_oracle.jsonl` <- `raw/longmemeval/longmemeval_oracle.json` (byte-identical in both HF repos).")
    L.append("- LongMemEval_M (`longmemeval_m_cleaned.json`, 2.74 GB < 3 GB) is downloaded to raw/ but not converted.")
    L.append("- `locomo.jsonl` <- `raw/locomo/locomo10.json` (github.com/snap-research/locomo).")
    L.append("- LongMemEval: 1 unit = 1 question with its own haystack; turn_id = `<session_id>:<turn_index>`; "
             "timestamp = session date; evidence_turn_ids = has_answer turns (any role) in answer sessions; "
             "evidence_session_ids = answer_session_ids.")
    L.append("- LoCoMo: 1 unit = 1 conversation (sample_id) with all its questions; turn_id = dia_id; "
             "session_id = `session_<N>`; timestamp = `session_<N>_date_time`; question_date = null; "
             "category 5 answer = \"Not mentioned in the conversation\" unless the raw item has an `answer` "
             "(2 items, both \"No\"); trap answer kept in `adversarial_answer`.")
    L.append("- session_index / turn_index are 0-based positions in raw file order. Timestamps are the raw strings.\n")
    L.append("## Overview\n")
    L.append("| dataset | units | questions | abstention q | turns | sessions (sum over units) | total chars | "
             "mean chars / unit | approx tokens total | approx tokens / unit (mean) | min-max chars / unit | "
             "q with empty evidence_turn_ids (non-abstention) | q with empty evidence_session_ids | output size |")
    L.append("|" + "---|" * 14)
    for name, s in stats.items():
        L.append(f"| {name} | {s['units']} | {s['questions']} | {s['abstention']} | {fmt_int(s['turns'])} | "
                 f"{fmt_int(s['sessions'])} | {fmt_int(s['chars_total'])} | {fmt_int(round(s['chars_mean']))} | "
                 f"{fmt_int(round(s['chars_total'] / 4))} | {fmt_int(round(s['chars_mean'] / 4))} | "
                 f"{fmt_int(s['chars_min'])}-{fmt_int(s['chars_max'])} | {s['empty_ev_turn']} "
                 f"({s['empty_ev_turn_nonabs']}) | {s['empty_ev_sess']} | {out_sizes[name] / 1e6:.1f} MB |")
    L.append("")
    L.append("## Questions by type\n")
    for name, s in stats.items():
        L.append(f"**{name}**\n")
        L.append("| question_type | questions | of which abstention | empty evidence_turn_ids |")
        L.append("|---|---|---|---|")
        for k, v in sorted(s["qtype"].items(), key=lambda kv: -kv[1]):
            L.append(f"| {k} | {v} | {s['qtype_abs'].get(k, 0)} | {s['qtype_empty'].get(k, 0)} |")
        L.append(f"| **total** | {s['questions']} | {s['abstention']} | {s['empty_ev_turn']} |\n")
    L.append("LongMemEval abstention = `question_id` ends with `_abs` (`question_type` keeps the underlying "
             "type). LongMemEval's own retrieval evaluation skips these. LoCoMo abstention = category 5 (adversarial).\n")
    L.append("LoCoMo category mapping used (released-code mapping): 1 = multi-hop, 2 = temporal, "
             "3 = open-domain, 4 = single-hop, 5 = adversarial. The raw number is kept in `raw_category`.\n")

    d = extras["lme_s_dedup"]
    L.append("## LongMemEval_S: distinct sessions across the 500 haystacks\n")
    L.append("| measure | count |")
    L.append("|---|---|")
    L.append(f"| session occurrences (sum of haystack lengths) | {fmt_int(d['occurrences'])} |")
    L.append(f"| distinct session ids | {fmt_int(d['distinct_ids'])} |")
    L.append(f"| distinct session contents (sha256 of [[role, content], ...]) | {fmt_int(d['distinct_hashes'])} |")
    L.append(f"| distinct `answer_*` (evidence) session ids | {fmt_int(d['distinct_answer_ids'])} |")
    L.append(f"| distinct filler session ids | {fmt_int(d['distinct_non_answer_ids'])} |")
    L.append(f"| session ids that map to >1 content | {d['ids_with_multiple_contents']} |")
    L.append(f"| contents that appear under >1 session id | {d['contents_with_multiple_ids']} |")
    L.append(f"| ...of which appear both as an `answer_*` session and under a filler id | "
             f"{d['contents_answer_and_filler']} |")
    L.append(f"| chars in the content-deduplicated pool | {fmt_int(d['unique_content_chars'])} "
             f"(~{fmt_int(d['unique_content_chars'] // 4)} tokens) |\n")
    if "lme_oracle_dedup" in extras:
        o = extras["lme_oracle_dedup"]
        L.append(f"(LongMemEval_oracle for comparison: {fmt_int(o['occurrences'])} occurrences, "
                 f"{fmt_int(o['distinct_ids'])} distinct ids, {fmt_int(o['distinct_hashes'])} distinct contents.)\n")

    L.append("## LongMemEval diagnostics\n")
    for name in ("longmemeval_s", "longmemeval_oracle"):
        x = extras[name + "_extra"]
        r = extras[name + "_report"]
        L.append(f"**{name}**\n")
        L.append(f"- evidence turns (has_answer == true) by role: {dict(x['evidence_turn_roles'])}; "
                 f"questions whose evidence turns are all assistant turns: {x['questions_evidence_only_assistant']}")
        L.append(f"- answer sessions with no has_answer turn: {x['answer_sessions_without_has_answer_turn']} "
                 f"(in {x['questions_with_such_answer_session']} questions)")
        L.append(f"- has_answer == true outside answer sessions: {r['has_answer_outside_answer_sessions']}; "
                 f"answer_session_ids missing from haystack: {len(r['missing_answer_sessions'])}")
        L.append(f"- haystacks with a session id repeated (renamed `<id>#dup1`): {len(r['dup_sessions'])}")
        L.append(f"- haystacks not in timestamp order: {x['haystacks_with_time_inversions']} "
                 f"(not in day order: {x['haystacks_with_day_inversions']})")
        L.append(f"- questions whose question_date is earlier than the latest haystack session: "
                 f"{x['question_before_last_session']} (same calendar day: {x['question_before_last_session_same_day']})")
        L.append(f"- turns with empty text: {stats[name]['empty_text_turns']}\n")

    if "lme_diff" in extras:
        df = extras["lme_diff"]
        L.append("## LongMemEval_S: cleaned (used) vs original/deprecated release\n")
        L.append(f"- question/answer/type/date/answer_session_ids changed: {dict(df['changed_fields']) or 'none'}")
        L.append(f"- questions whose haystack lost sessions: {df['questions_with_removed_sessions']} / {df['questions']}; "
                 f"sessions removed in total: {df['removed_total']} (of which empty sessions: {df['removed_empty']}); "
                 f"sessions added: {df['added_total']}")
        L.append(f"- removed sessions per question: {df['removed_per_question']}")
        L.append(f"- turns: original {fmt_int(df['orig_turns'])} -> cleaned {fmt_int(df['clean_turns'])}\n")

    r = extras["locomo_report"]
    L.append("## LoCoMo evidence normalisation\n")
    log = r["evidence_log"]
    raw_bad = sorted({(e[1], e[3]) for e in log})
    L.append(f"- raw evidence strings that are not an exact dia_id: {len(raw_bad)}")
    L.append(f"- resolved by splitting / fixing: {sum(1 for e in log if e[5] in ('split', 'fixed'))} ids; "
             f"unresolvable ids dropped: {sum(1 for e in log if e[5] == 'unmatched')}")
    L.append(f"- evidence lists containing duplicate ids (deduplicated): {r['dup_evidence_lists']}")
    L.append(f"- session_N_date_time keys without a session_N (ignored): {len(r['orphan_dates'])} "
             f"({', '.join(sorted({s for s, _ in r['orphan_dates']}))})\n")
    L.append("| qid | category | raw evidence string | token | status | resolved to |")
    L.append("|---|---|---|---|---|---|")
    for e in log:
        L.append(f"| {e[1]} | {e[2]} | `{e[3]}` | `{e[4]}` | {e[5]} | {e[6] or ''} |")
    L.append("")
    lc = extras["locomo_captions"]
    L.append(f"LoCoMo turns with an `image_caption`: {lc[0]} ({fmt_int(lc[1])} caption chars, not in the totals above).\n")

    L.append("## Downloads (raw/)\n")
    L.append("| file | source | revision | bytes | sha256 |")
    L.append("|---|---|---|---|---|")
    for f in manifest.get("files", []):
        lp = f.get("local_path")
        rel = os.path.relpath(lp, root) if lp else "(not downloaded)"
        size = f.get("local_size", f.get("remote_size"))
        note = f" ({f['skipped']})" if f.get("skipped") else ""
        L.append(f"| {rel}{note} | {f['repo']}/{f['remote_path']} | {f['revision'][:12]} | "
                 f"{fmt_int(size) if size else '?'} | {(f.get('local_sha256') or f.get('remote_sha256') or '')[:16]} |")
    listing = manifest.get("longmemeval_listing", {})
    if listing:
        L.append("\nHF repo listings at download time (bytes):\n")
        for repo, v in listing.items():
            L.append(f"- {repo}@{v['revision'][:12]}: " + ", ".join(
                f"{k} {fmt_int(x['size'])}" for k, x in sorted(v["files"].items()) if not k.startswith(".")))
    L.append("")
    with open(os.path.join(root, "STATS.md"), "w") as f:
        f.write("\n".join(L))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default="/home/user/data/memory")
    ap.add_argument("--lme-m-every", type=int, default=0, help="only convert every n-th LongMemEval_M question to longmemeval_m_every<n>.jsonl")
    args = ap.parse_args()
    root = args.root
    if args.lme_m_every:
        out = os.path.join(root, f"longmemeval_m_every{args.lme_m_every}.jsonl")
        n = convert_longmemeval_m(os.path.join(root, "raw", "longmemeval", "longmemeval_m_cleaned.json"), out, args.lme_m_every)
        print(f"wrote {n} units to {out}")
        return
    raw = os.path.join(root, "raw")
    src = {
        "longmemeval_s": os.path.join(raw, "longmemeval", "longmemeval_s_cleaned.json"),
        "longmemeval_oracle": os.path.join(raw, "longmemeval", "longmemeval_oracle.json"),
        "locomo": os.path.join(raw, "locomo", "locomo10.json"),
    }
    orig_s = os.path.join(raw, "longmemeval_original_deprecated", "longmemeval_s.json")
    stats, out_sizes, extras = {}, {}, {}

    for name in ("longmemeval_s", "longmemeval_oracle"):
        rep = {}
        raw_data, units = convert_longmemeval(src[name], name, rep)
        out = os.path.join(root, name + ".jsonl")
        out_sizes[name] = write_jsonl(units, out)
        assert validate(out) == len(units)
        stats[name] = unit_stats(units)
        extras[name + "_report"] = rep
        extras[name + "_extra"] = lme_extra(raw_data)
        if name == "longmemeval_s":
            extras["lme_s_dedup"] = lme_session_dedup(raw_data)
            if os.path.exists(orig_s):
                extras["lme_diff"] = lme_diff(raw_data, json.load(open(orig_s)))
        else:
            extras["lme_oracle_dedup"] = lme_session_dedup(raw_data)
        del raw_data, units
        print(f"wrote {out} ({out_sizes[name]} bytes)")

    rep = {}
    _, units = convert_locomo(src["locomo"], rep)
    out = os.path.join(root, "locomo.jsonl")
    out_sizes["locomo"] = write_jsonl(units, out)
    assert validate(out) == len(units)
    stats["locomo"] = unit_stats(units)
    extras["locomo_report"] = rep
    caps = [t["image_caption"] for u in units for t in u["turns"] if "image_caption" in t]
    extras["locomo_captions"] = (len(caps), sum(len(c) for c in caps))
    print(f"wrote {out} ({out_sizes['locomo']} bytes)")

    manifest = {}
    mp = os.path.join(raw, "MANIFEST.json")
    if os.path.exists(mp):
        manifest = json.load(open(mp))
    write_stats(root, stats, manifest, out_sizes, extras)
    print("wrote", os.path.join(root, "STATS.md"))
    json.dump({k: {kk: (dict(vv) if isinstance(vv, collections.Counter) else vv) for kk, vv in v.items()}
               for k, v in stats.items()}, sys.stdout, indent=1, default=str)
    print()


if __name__ == "__main__":
    main()
