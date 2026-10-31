# Memory bench: slotwise

Model haiku; 80 runs. Solved = score 1.0. Costs in USD per run; memory $ = the memory system's own model calls during the run; history $ = agent + memory system while recording the history.

| lane | runs | solved | score | control solved | conversation solved | repo solved | $/run | memory $/run | steps | wall s | memory tool calls | history $ (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| engram | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.007 | 0.002 | 7.6 | 23 | 0.3 | 0.06 + 0.03 | – / – | – / – |
| engram+next | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.007 | 0.002 | 7.0 | 21 | 0.3 | 0.06 + 0.03 | – / – | – / – |

Against the base lane (paired when the lane is a --variant):

| lane | base | solved | base solved | p (Fisher / perm) |
|---|---|---|---|---|
| engram+next | engram | 40/40 | 40/40 | 1.000 / 1.000 |

Per task (mean score):

| task | kind | engram | engram+next |
|---|---|---|---|
| cancellation_fee | conversation | 1.00 | 1.00 |
| fully_booked | conversation | 1.00 | 1.00 |
| day_plan | conversation | 1.00 | 1.00 |
| series_booking | conversation | 1.00 | 1.00 |
| sms_reminder | conversation | 1.00 | 1.00 |
| ical_advice | conversation | 1.00 | 1.00 |
| reschedule | repo | 1.00 | 1.00 |
| free_slots | repo | 1.00 | 1.00 |
| parse_duration | control | 1.00 | 1.00 |
| merge_intervals | control | 1.00 | 1.00 |

p: two-sided. Fisher = Fisher's exact test on solved runs pooled over tasks; perm = permutation test of the mean score difference with lane labels shuffled within each task (scripts/ab_stats.py).
