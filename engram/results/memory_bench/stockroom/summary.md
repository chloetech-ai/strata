# Memory bench: stockroom

Model haiku; 300 runs. Solved = score 1.0. Costs in USD per run; memory $ = the memory system's own model calls during the run; history $ = agent + memory system while recording the history.

| lane | runs | solved | score | control solved | conversation solved | repo solved | $/run | memory $/run | steps | wall s | memory tool calls | history $ (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| none | 30 | 14/30 | 0.61 | 6/6 | 2/18 | 6/6 | 0.004 | 0.000 | 7.7 | 28 | 0.0 | 0.05 + 0.00 | – / – | 0.000 / 0.000 |
| ccmem | 30 | 23/30 | 0.87 | 6/6 | 11/18 | 6/6 | 0.004 | 0.000 | 7.2 | 29 | 0.0 | 0.05 + 0.00 | 0.033 / 0.000 | 0.011 / 0.003 |
| transcripts | 30 | 25/30 | 0.87 | 6/6 | 13/18 | 6/6 | 0.005 | 0.000 | 7.7 | 28 | 0.0 | 0.05 + 0.00 | 0.006 / 0.000 | 0.052 / 0.020 |
| claude-mem | 30 | 24/30 | 0.88 | 6/6 | 12/18 | 6/6 | 0.006 | 0.008 | 6.7 | 34 | 0.2 | 0.06 + 0.09 | 0.015 / 0.000 | 0.024 / 0.005 |
| mem0 | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.005 | 0.000 | 8.9 | 36 | 1.5 | 0.06 + 0.01 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram@main | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.005 | 0.000 | 6.0 | 31 | 0.1 | 0.05 + 0.02 | 0.000 / 0.000 | – / – |
| engram | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.006 | 0.003 | 6.6 | 33 | 0.1 | 0.06 + 0.04 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram+deep | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.005 | 0.003 | 6.0 | 42 | 0.0 | 0.05 + 0.04 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram+nodeep | 30 | 29/30 | 0.98 | 6/6 | 17/18 | 6/6 | 0.005 | 0.000 | 6.1 | 29 | 0.1 | 0.05 + 0.02 | 0.000 / 0.000 | 1.000 / 1.000 |
| full | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.003 | 0.000 | 4.1 | 22 | 0.0 | 0.03 + 0.00 | 0.000 / 0.000 | 1.000 / 1.000 |

Per task (mean score):

| task | kind | none | ccmem | transcripts | claude-mem | mem0 | engram@main | engram | engram+deep | engram+nodeep | full |
|---|---|---|---|---|---|---|---|---|---|---|---|
| count_sheet | conversation | 0.44 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| stock_value | conversation | 0.83 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| purchase_suggestions | conversation | 0.67 | 0.67 | 1.00 | 0.67 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| scanner_import | conversation | 0.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| negative_stock_advice | conversation | 0.00 | 0.83 | 0.33 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 0.83 | 1.00 |
| config_advice | conversation | 0.17 | 0.17 | 0.33 | 0.17 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| last_receipt | repo | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| write_off | repo | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| abc_classes | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| parse_location | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |

p: two-sided. Fisher = Fisher's exact test on solved runs pooled over tasks; perm = permutation test of the mean score difference with lane labels shuffled within each task (scripts/ab_stats.py).
