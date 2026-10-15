# Memory bench: slotwise

Model haiku; 300 runs. Solved = score 1.0. Costs in USD per run; memory $ = the memory system's own model calls during the run; history $ = agent + memory system while recording the history.

| lane | runs | solved | score | control solved | conversation solved | repo solved | $/run | memory $/run | steps | wall s | memory tool calls | history $ (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| none | 30 | 12/30 | 0.63 | 6/6 | 0/18 | 6/6 | 0.006 | 0.000 | 8.8 | 34 | 0.0 | 0.04 + 0.00 | – / – | 0.000 / 0.000 |
| ccmem | 30 | 24/30 | 0.90 | 6/6 | 12/18 | 6/6 | 0.006 | 0.000 | 9.0 | 36 | 0.0 | 0.04 + 0.00 | 0.003 / 0.000 | 0.024 / 0.004 |
| transcripts | 30 | 25/30 | 0.92 | 6/6 | 13/18 | 6/6 | 0.007 | 0.000 | 10.5 | 38 | 0.0 | 0.05 + 0.00 | 0.001 / 0.000 | 0.052 / 0.022 |
| claude-mem | 30 | 21/30 | 0.85 | 6/6 | 9/18 | 6/6 | 0.008 | 0.009 | 8.4 | 47 | 0.3 | 0.05 + 0.09 | 0.037 / 0.000 | 0.002 / 0.000 |
| mem0 | 30 | 27/30 | 0.95 | 6/6 | 15/18 | 6/6 | 0.006 | 0.000 | 10.8 | 42 | 1.4 | 0.04 + 0.01 | 0.000 / 0.000 | 0.237 / 0.095 |
| engram@main | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.007 | 0.000 | 7.8 | 36 | 0.3 | 0.05 + 0.02 | 0.000 / 0.000 | – / – |
| engram | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.007 | 0.003 | 8.1 | 40 | 0.1 | 0.06 + 0.05 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram+deep | 30 | 27/30 | 0.90 | 6/6 | 15/18 | 6/6 | 0.007 | 0.003 | 6.8 | 45 | 0.0 | 0.07 + 0.05 | 0.000 / 0.000 | 0.237 / 0.096 |
| engram+nodeep | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.007 | 0.000 | 7.5 | 35 | 0.1 | 0.05 + 0.02 | 0.000 / 0.000 | 1.000 / 1.000 |
| full | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.004 | 0.000 | 5.2 | 25 | 0.0 | 0.03 + 0.00 | 0.000 / 0.000 | 1.000 / 1.000 |

Per task (mean score):

| task | kind | none | ccmem | transcripts | claude-mem | mem0 | engram@main | engram | engram+deep | engram+nodeep | full |
|---|---|---|---|---|---|---|---|---|---|---|---|
| cancellation_fee | conversation | 0.33 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 0.00 | 1.00 | 1.00 |
| fully_booked | conversation | 0.50 | 0.50 | 0.67 | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| day_plan | conversation | 0.50 | 0.50 | 1.00 | 0.50 | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| series_booking | conversation | 0.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| sms_reminder | conversation | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| ical_advice | conversation | 0.50 | 1.00 | 0.50 | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| reschedule | repo | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| free_slots | repo | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| parse_duration | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| merge_intervals | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |

p: two-sided. Fisher = Fisher's exact test on solved runs pooled over tasks; perm = permutation test of the mean score difference with lane labels shuffled within each task (scripts/ab_stats.py).
