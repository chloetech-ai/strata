# Memory bench: bakehouse

Model haiku; 144 runs. Solved = score 1.0. Costs in USD per run; memory $ = the memory system's own model calls during the run; history $ = agent + memory system while recording the history.

| lane | runs | solved | score | control solved | inference solved | recall solved | revised solved | stale solved | where_why solved | $/run | memory $/run | steps | wall s | memory tool calls | history $ (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| engram | 72 | 67/72 | 0.97 | 8/8 | 12/16 | 4/4 | 15/16 | 20/20 | 8/8 | 0.006 | 0.002 | 7.4 | 20 | 0.2 | 0.21 + 0.12 | – / – | – / – |
| engram+next | 72 | 65/72 | 0.95 | 8/8 | 12/16 | 4/4 | 13/16 | 20/20 | 8/8 | 0.006 | 0.002 | 7.2 | 20 | 0.1 | 0.21 + 0.12 | – / – | – / – |

Against the base lane (paired when the lane is a --variant):

| lane | base | solved | base solved | p (Fisher / perm) |
|---|---|---|---|---|
| engram+next | engram | 65/72 | 67/72 | 0.764 / 0.256 |

Per task (mean score):

| task | kind | engram | engram+next |
|---|---|---|---|
| delivery_charge | revised | 1.00 | 1.00 |
| price_rise | revised | 1.00 | 1.00 |
| bake_sheet | revised | 0.92 | 0.75 |
| crate_labels | revised | 1.00 | 1.00 |
| extend_cutoff | stale | 1.00 | 1.00 |
| next_delivery_day | stale | 1.00 | 1.00 |
| relief_baker_note | stale | 1.00 | 1.00 |
| cutoff_quick | stale | 1.00 | 1.00 |
| closed_day_quick | stale | 1.00 | 1.00 |
| owner_weekly_file | inference | 1.00 | 1.00 |
| vat_export | inference | 1.00 | 1.00 |
| van_list | inference | 0.62 | 0.44 |
| school_order | inference | 1.00 | 1.00 |
| returns_credit | recall | 1.00 | 1.00 |
| pence_why | where_why | 1.00 | 1.00 |
| unknown_code_why | where_why | 1.00 | 1.00 |
| easter_sunday | control | 1.00 | 1.00 |
| baker_percentages | control | 1.00 | 1.00 |

p: two-sided. Fisher = Fisher's exact test on solved runs pooled over tasks; perm = permutation test of the mean score difference with lane labels shuffled within each task (scripts/ab_stats.py).
