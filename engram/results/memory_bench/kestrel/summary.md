# Memory bench: kestrel

Model haiku; 420 runs. Solved = score 1.0. Costs in USD per run; memory $ = the memory system's own model calls during the run; history $ = agent + memory system while recording the history.

| lane | runs | solved | score | control solved | conversation solved | repo solved | $/run | memory $/run | steps | wall s | memory tool calls | history $ (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| none | 42 | 20/42 | 0.63 | 6/6 | 11/33 | 3/3 | 0.005 | 0.000 | 7.7 | 26 | 0.0 | 0.10 + 0.00 | – / – | 0.000 / 0.000 |
| ccmem | 42 | 29/42 | 0.84 | 6/6 | 20/33 | 3/3 | 0.007 | 0.000 | 9.3 | 34 | 0.0 | 0.10 + 0.00 | 0.076 / 0.000 | 0.011 / 0.002 |
| transcripts | 42 | 36/42 | 0.94 | 6/6 | 27/33 | 3/3 | 0.007 | 0.000 | 9.2 | 33 | 0.0 | 0.10 + 0.00 | 0.000 / 0.000 | 0.483 / 0.450 |
| claude-mem | 42 | 24/42 | 0.68 | 6/6 | 15/33 | 3/3 | 0.007 | 0.008 | 6.8 | 25 | 0.1 | 0.11 + 0.16 | 0.512 / 0.211 | 0.000 / 0.000 |
| mem0 | 42 | 37/42 | 0.92 | 6/6 | 28/33 | 3/3 | 0.006 | 0.000 | 10.4 | 33 | 1.5 | 0.11 + 0.01 | 0.000 / 0.000 | 0.713 / 0.104 |
| engram@main | 42 | 39/42 | 0.96 | 6/6 | 30/33 | 3/3 | 0.007 | 0.000 | 7.2 | 34 | 0.2 | 0.13 + 0.05 | 0.000 / 0.000 | – / – |
| engram | 42 | 39/42 | 0.98 | 6/6 | 30/33 | 3/3 | 0.007 | 0.003 | 7.0 | 32 | 0.3 | 0.16 + 0.12 | 0.000 / 0.000 | 1.000 / 0.404 |
| engram+deep | 42 | 39/42 | 0.94 | 6/6 | 30/33 | 3/3 | 0.007 | 0.003 | 6.5 | 42 | 0.1 | 0.15 + 0.12 | 0.000 / 0.000 | 1.000 / 0.407 |
| engram+nodeep | 42 | 38/42 | 0.93 | 6/6 | 29/33 | 3/3 | 0.007 | 0.000 | 6.5 | 29 | 0.1 | 0.14 + 0.06 | 0.000 / 0.000 | 1.000 / 0.203 |
| full | 42 | 39/42 | 0.94 | 6/6 | 30/33 | 3/3 | 0.030 | 0.000 | 5.2 | 24 | 0.0 | 0.24 + 0.00 | 0.000 / 0.000 | 1.000 / 0.399 |

Per task (mean score):

| task | kind | none | ccmem | transcripts | claude-mem | mem0 | engram@main | engram | engram+deep | engram+nodeep | full |
|---|---|---|---|---|---|---|---|---|---|---|---|
| free_slots | conversation | 0.28 | 0.67 | 1.00 | 0.17 | 0.94 | 1.00 | 1.00 | 1.00 | 0.89 | 1.00 |
| can_book | conversation | 0.07 | 0.60 | 0.60 | 0.07 | 0.20 | 0.47 | 0.73 | 0.20 | 0.20 | 0.20 |
| cancellation_fee | conversation | 0.17 | 1.00 | 1.00 | 0.33 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| appointment_price | conversation | 0.00 | 0.17 | 1.00 | 0.17 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| reminder_text | conversation | 0.89 | 0.89 | 1.00 | 0.89 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| send_reminders | conversation | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| invoice_number | conversation | 1.00 | 1.00 | 1.00 | 1.00 | 0.67 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| invoice_export | conversation | 0.56 | 1.00 | 0.72 | 0.56 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| waitlist_offer | conversation | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| overbooking_advice | conversation | 0.33 | 1.00 | 0.83 | 0.33 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| python_advice | conversation | 0.50 | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| upcoming | repo | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| merge_busy | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| parse_duration | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |

p: two-sided. Fisher = Fisher's exact test on solved runs pooled over tasks; perm = permutation test of the mean score difference with lane labels shuffled within each task (scripts/ab_stats.py).
