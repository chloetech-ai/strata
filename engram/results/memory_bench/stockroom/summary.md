# Memory bench: stockroom

Model haiku; 450 runs. Solved = score 1.0. Costs in USD per run; memory $ = the memory system's own model calls during the run; history $ = agent + memory system while recording the history.

| lane | runs | solved | score | control solved | conversation solved | repo solved | $/run | memory $/run | steps | wall s | memory tool calls | history $ (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| none | 40 | 18/40 | 0.61 | 8/8 | 2/24 | 8/8 | 0.004 | 0.000 | 7.4 | 28 | 0.0 | 0.05 + 0.00 | – / – | 0.000 / 0.000 |
| ccmem | 40 | 30/40 | 0.85 | 8/8 | 14/24 | 8/8 | 0.005 | 0.000 | 7.0 | 28 | 0.0 | 0.05 + 0.00 | 0.012 / 0.000 | 0.001 / 0.000 |
| transcripts | 40 | 34/40 | 0.88 | 8/8 | 18/24 | 8/8 | 0.005 | 0.000 | 7.7 | 27 | 0.0 | 0.05 + 0.00 | 0.000 / 0.000 | 0.026 / 0.007 |
| claude-mem | 40 | 33/40 | 0.90 | 8/8 | 17/24 | 8/8 | 0.006 | 0.007 | 6.7 | 34 | 0.2 | 0.06 + 0.09 | 0.001 / 0.000 | 0.012 / 0.001 |
| mem0 | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.005 | 0.000 | 8.8 | 36 | 1.4 | 0.06 + 0.01 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram@main | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.005 | 0.000 | 6.1 | 31 | 0.1 | 0.05 + 0.02 | 0.000 / 0.000 | – / – |
| engram | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.006 | 0.003 | 6.6 | 33 | 0.1 | 0.06 + 0.04 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram+deep | 30 | 30/30 | 1.00 | 6/6 | 18/18 | 6/6 | 0.005 | 0.003 | 6.0 | 42 | 0.0 | 0.05 + 0.04 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram+nodeep | 30 | 29/30 | 0.98 | 6/6 | 17/18 | 6/6 | 0.005 | 0.000 | 6.1 | 29 | 0.1 | 0.05 + 0.02 | 0.000 / 0.000 | 0.429 / 0.430 |
| full | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.005 | 0.000 | 4.1 | 21 | 0.0 | 0.03 + 0.00 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram+rank | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.006 | 0.001 | 6.6 | 24 | 0.2 | 0.06 + 0.02 | 0.000 / 0.000 | 1.000 / 1.000 |
| engram+v5 | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.006 | 0.002 | 6.6 | 24 | 0.1 | 0.06 + 0.03 | 0.000 / 0.000 | 1.000 / 1.000 |

Per task (mean score):

| task | kind | none | ccmem | transcripts | claude-mem | mem0 | engram@main | engram | engram+deep | engram+nodeep | full | engram+rank | engram+v5 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| count_sheet | conversation | 0.42 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| stock_value | conversation | 0.75 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| purchase_suggestions | conversation | 0.67 | 0.67 | 1.00 | 0.67 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| scanner_import | conversation | 0.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| negative_stock_advice | conversation | 0.00 | 0.75 | 0.50 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 0.83 | 1.00 | 1.00 | 1.00 |
| config_advice | conversation | 0.25 | 0.12 | 0.25 | 0.38 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| last_receipt | repo | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| write_off | repo | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| abc_classes | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| parse_location | control | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |

p: two-sided. Fisher = Fisher's exact test on solved runs pooled over tasks; perm = permutation test of the mean score difference with lane labels shuffled within each task (scripts/ab_stats.py).
