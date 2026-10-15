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
