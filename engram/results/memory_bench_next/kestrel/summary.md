# Memory bench: kestrel

Model haiku; 112 runs. Solved = score 1.0. Costs in USD per run; memory $ = the memory system's own model calls during the run; history $ = agent + memory system while recording the history.

| lane | runs | solved | score | control solved | conversation solved | repo solved | $/run | memory $/run | steps | wall s | memory tool calls | history $ (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| engram | 56 | 52/56 | 0.96 | 8/8 | 40/44 | 4/4 | 0.008 | 0.002 | 7.2 | 25 | 0.2 | 0.15 + 0.08 | – / – | – / – |
| engram+next | 56 | 52/56 | 0.94 | 8/8 | 40/44 | 4/4 | 0.008 | 0.002 | 7.3 | 26 | 0.1 | 0.15 + 0.08 | – / – | – / – |

Against the base lane (paired when the lane is a --variant):

| lane | base | solved | base solved | p (Fisher / perm) |
|---|---|---|---|---|
| engram+next | engram | 52/56 | 52/56 | 1.000 / 0.311 |

Per task (mean score):

| task | kind | engram | engram+next |
|---|---|---|---|
| free_slots | conversation | 1.00 | 1.00 |
| can_book | conversation | 0.40 | 0.15 |
| cancellation_fee | conversation | 1.00 | 1.00 |
| appointment_price | conversation | 1.00 | 1.00 |
| reminder_text | conversation | 1.00 | 1.00 |
| send_reminders | conversation | 1.00 | 1.00 |
| invoice_number | conversation | 1.00 | 1.00 |
| invoice_export | conversation | 1.00 | 1.00 |
| waitlist_offer | conversation | 1.00 | 1.00 |
| overbooking_advice | conversation | 1.00 | 1.00 |
| python_advice | conversation | 1.00 | 1.00 |
| upcoming | repo | 1.00 | 1.00 |
| merge_busy | control | 1.00 | 1.00 |
| parse_duration | control | 1.00 | 1.00 |

p: two-sided. Fisher = Fisher's exact test on solved runs pooled over tasks; perm = permutation test of the mean score difference with lane labels shuffled within each task (scripts/ab_stats.py).
