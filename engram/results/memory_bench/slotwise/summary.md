# Memory bench: slotwise

Model haiku; 450 runs. Solved = score 1.0. Costs in USD per run; memory $ = the memory system's own model calls during the run; history $ = agent + memory system while recording the history.

| lane | runs | solved | score | control solved | conversation solved | repo solved | $/run | memory $/run | steps | wall s | memory tool calls | history $ (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| none | 40 | 16/40 | 0.63 | 8/8 | 0/24 | 8/8 | 0.006 | 0.000 | 8.7 | 33 | 0.0 | 0.04 + 0.00 | – / – | 0.000 / 0.000 |
| ccmem | 40 | 33/40 | 0.91 | 8/8 | 17/24 | 8/8 | 0.006 | 0.000 | 8.8 | 35 | 0.0 | 0.04 + 0.00 | 0.000 / 0.000 | 0.012 / 0.002 |
| transcripts | 40 | 32/40 | 0.89 | 8/8 | 16/24 | 8/8 | 0.007 | 0.000 | 10.1 | 37 | 0.0 | 0.05 + 0.00 | 0.001 / 0.000 | 0.005 / 0.001 |
| claude-mem | 40 | 28/40 | 0.85 | 8/8 | 12/24 | 8/8 | 0.009 | 0.010 | 8.4 | 59 | 0.4 | 0.05 + 0.09 | 0.013 / 0.000 | 0.000 / 0.000 |
| mem0 | 40 | 36/40 | 0.95 | 8/8 | 20/24 | 8/8 | 0.006 | 0.000 | 11.1 | 43 | 1.5 | 0.04 + 0.01 | 0.000 / 0.000 | 0.116 / 0.028 |
| engram@main | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.007 | 0.000 | 7.5 | 35 | 0.2 | 0.05 + 0.02 | 0.000 / 0.000 | – / – |
| engram | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.007 | 0.003 | 8.1 | 40 | 0.1 | 0.06 + 0.05 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram+deep | 30 | 27/30 | 0.90 | 6/6 | 15/18 | 6/6 | 0.007 | 0.003 | 6.8 | 45 | 0.0 | 0.07 + 0.05 | 0.000 / 0.000 | 0.074 / 0.028 |
| engram+nodeep | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.007 | 0.000 | 7.5 | 35 | 0.1 | 0.05 + 0.02 | 0.000 / 0.000 | 1.000 / 1.000 |
| full | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.006 | 0.000 | 5.3 | 24 | 0.0 | 0.03 + 0.00 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram+v5 | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.007 | 0.002 | 7.7 | 30 | 0.1 | 0.05 + 0.03 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram+rank | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.007 | 0.001 | 7.3 | 29 | 0.1 | 0.06 + 0.03 | 0.000 / 0.000 | 1.000 / 1.000 |

Per task (mean score):

| task | kind | none | ccmem | transcripts | claude-mem | mem0 | engram@main | engram | engram+deep | engram+nodeep | full | engram+v5 | engram+rank |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| cancellation_fee | conversation | 0.33 | 1.00 | 0.75 | 1.00 | 1.00 | 1.00 | 1.00 | 0.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| fully_booked | conversation | 0.50 | 0.62 | 0.62 | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| day_plan | conversation | 0.50 | 0.50 | 1.00 | 0.50 | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| series_booking | conversation | 0.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| sms_reminder | conversation | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| ical_advice | conversation | 0.50 | 1.00 | 0.50 | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| reschedule | repo | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| free_slots | repo | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| parse_duration | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| merge_intervals | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |

p: two-sided. Fisher = Fisher's exact test on solved runs pooled over tasks; perm = permutation test of the mean score difference with lane labels shuffled within each task (scripts/ab_stats.py).
