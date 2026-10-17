# Memory bench: kestrel

Model haiku; 686 runs. Solved = score 1.0. Costs in USD per run; memory $ = the memory system's own model calls during the run; history $ = agent + memory system while recording the history.

| lane | runs | solved | score | control solved | conversation solved | repo solved | $/run | memory $/run | steps | wall s | memory tool calls | history $ (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| none | 56 | 27/56 | 0.63 | 8/8 | 15/44 | 4/4 | 0.005 | 0.000 | 7.7 | 27 | 0.0 | 0.10 + 0.00 | – / – | 0.000 / 0.000 |
| ccmem | 56 | 39/56 | 0.85 | 8/8 | 27/44 | 4/4 | 0.007 | 0.000 | 9.0 | 34 | 0.0 | 0.10 + 0.00 | 0.034 / 0.000 | 0.003 / 0.000 |
| transcripts | 56 | 48/56 | 0.94 | 8/8 | 36/44 | 4/4 | 0.008 | 0.000 | 9.3 | 35 | 0.0 | 0.10 + 0.00 | 0.000 / 0.000 | 0.360 / 0.274 |
| claude-mem | 56 | 33/56 | 0.70 | 8/8 | 21/44 | 4/4 | 0.007 | 0.008 | 7.0 | 30 | 0.1 | 0.11 + 0.16 | 0.344 / 0.041 | 0.000 / 0.000 |
| mem0 | 56 | 50/56 | 0.92 | 8/8 | 38/44 | 4/4 | 0.007 | 0.000 | 10.3 | 36 | 1.6 | 0.11 + 0.01 | 0.000 / 0.000 | 0.742 / 0.034 |
| engram@main | 56 | 52/56 | 0.97 | 8/8 | 40/44 | 4/4 | 0.007 | 0.000 | 7.0 | 34 | 0.2 | 0.13 + 0.05 | 0.000 / 0.000 | – / – |
| engram | 42 | 39/42 | 0.98 | 6/6 | 30/33 | 3/3 | 0.007 | 0.003 | 7.0 | 32 | 0.3 | 0.16 + 0.12 | 0.000 / 0.000 | 1.000 / 0.423 |
| engram+deep | 42 | 39/42 | 0.94 | 6/6 | 30/33 | 3/3 | 0.007 | 0.003 | 6.5 | 42 | 0.1 | 0.15 + 0.12 | 0.000 / 0.000 | 1.000 / 0.249 |
| engram+nodeep | 42 | 38/42 | 0.93 | 6/6 | 29/33 | 3/3 | 0.007 | 0.000 | 6.5 | 29 | 0.1 | 0.14 + 0.06 | 0.000 / 0.000 | 0.721 / 0.068 |
| full | 56 | 52/56 | 0.94 | 8/8 | 40/44 | 4/4 | 0.055 | 0.000 | 5.4 | 25 | 0.0 | 0.24 + 0.00 | 0.000 / 0.000 | 1.000 / 0.147 |
| engram+v5 | 56 | 52/56 | 0.99 | 8/8 | 40/44 | 4/4 | 0.008 | 0.002 | 7.2 | 33 | 0.1 | 0.15 + 0.09 | 0.000 / 0.000 | 1.000 / 0.436 |
| engram+rank | 56 | 51/56 | 0.98 | 8/8 | 39/44 | 4/4 | 0.008 | 0.001 | 7.1 | 32 | 0.2 | 0.17 + 0.07 | 0.000 / 0.000 | 1.000 / 0.421 |
| engram+v7 | 56 | 49/56 | 0.93 | 8/8 | 37/44 | 4/4 | 0.007 | 0.002 | 6.9 | 30 | 0.1 | 0.15 + 0.08 | 0.000 / 0.000 | 0.527 / 0.009 |

Per task (mean score):

| task | kind | none | ccmem | transcripts | claude-mem | mem0 | engram@main | engram | engram+deep | engram+nodeep | full | engram+v5 | engram+rank | engram+v7 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| free_slots | conversation | 0.25 | 0.67 | 1.00 | 0.29 | 0.96 | 1.00 | 1.00 | 1.00 | 0.89 | 1.00 | 1.00 | 0.92 | 0.88 |
| can_book | conversation | 0.05 | 0.65 | 0.65 | 0.05 | 0.20 | 0.55 | 0.73 | 0.20 | 0.20 | 0.20 | 0.80 | 0.80 | 0.20 |
| cancellation_fee | conversation | 0.25 | 1.00 | 1.00 | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| appointment_price | conversation | 0.00 | 0.12 | 1.00 | 0.19 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| reminder_text | conversation | 0.92 | 0.92 | 1.00 | 0.92 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| send_reminders | conversation | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| invoice_number | conversation | 1.00 | 1.00 | 1.00 | 1.00 | 0.75 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| invoice_export | conversation | 0.54 | 1.00 | 0.79 | 0.54 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| waitlist_offer | conversation | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| overbooking_advice | conversation | 0.38 | 1.00 | 0.75 | 0.38 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| python_advice | conversation | 0.50 | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| upcoming | repo | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| merge_busy | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| parse_duration | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |

p: two-sided. Fisher = Fisher's exact test on solved runs pooled over tasks; perm = permutation test of the mean score difference with lane labels shuffled within each task (scripts/ab_stats.py).
