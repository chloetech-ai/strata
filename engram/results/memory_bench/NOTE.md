# Runs re-done after a usage limit

Between 09:13 and 10:50 UTC on 2026-10-15 the account these runs used hit its
usage limit. Every `claude` call in that window failed at once with "You've
hit your session limit", so its runs were recorded as errors with no cost and
a score of 0: 8 on bakehouse, 15 on slotwise and 118 on kestrel, across every
lane. The last block of kestrel's claude-mem history failed the same way.

Those rows were removed from `runs.jsonl`, kestrel's claude-mem history and
runs were deleted, and `scripts/memory_bench.py all` was run again with the
same lanes and binaries. It records a missing history and runs only the
missing (lane, task, repeat) rows, so every row now comes from a run that
completed. No run that completed was removed or re-run. All 1,500 rows have
`error: false`.

`pooled.md` and `pooled.json` (from `scripts/v2_charts.py`) pool the four
projects, with tasks as strata for the permutation test.

## Round 2

The fourth repeat and the two new lanes (`engram+v5`, the final build's
default; `engram+rank`, the same with `ENGRAM_DEEP_PLAN=none`) ran from 14:45
to 16:20 UTC. The account hit its usage limit again from 15:47 to 15:50: 131
runs on stockroom, slotwise and kestrel failed the same way. They were
removed and re-run as above. The first attempt at kestrel's `engram+rank`
history failed before it started (its binary was still being copied) and
was recorded again from scratch.

Bakehouse's two quick questions (`cutoff_quick`, `closed_day_quick`) were
first graded too strictly: an answer that gave the new value and also
mentioned the old one as out of date failed. The graders were fixed and
`memory_bench.py regrade` re-scored the stored answers; 13 rows changed, and
each keeps its first score in `regraded_from`.
