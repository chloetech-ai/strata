#!/usr/bin/env python3
"""Writes DATA/NAME.shifted.jsonl: DATA/NAME.jsonl with each ts moved by
the shift ChloeDB's load applied (common.shift) and written as RFC 3339
with microseconds, which VictoriaLogs and OpenSearch's date_nanos read
exactly. Done before the timed loads.
  shift.py DATA NAME...
"""
import os
import re
import sys

import common as c

TS = re.compile(rb'"ts":(-?\d+)')

for name in sys.argv[2:]:
    src = os.path.join(sys.argv[1], name + ".jsonl")
    dst = os.path.join(sys.argv[1], name + ".shifted.jsonl")
    if os.path.exists(dst):
        continue
    sh = c.shift(src, name)
    sub = lambda m: b'"ts":"' + c.iso(int(m.group(1)) + sh).encode() + b'"'
    with open(src, "rb") as f, open(dst + ".tmp", "wb") as out:
        for line in f:
            out.write(TS.sub(sub, line, count=1))
    os.rename(dst + ".tmp", dst)
    print(name, "shift_us", sh)
