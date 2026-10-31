# Memory bench: stockroom

Model haiku; 80 runs. Solved = score 1.0. Costs in USD per run; memory $ = the memory system's own model calls during the run; history $ = agent + memory system while recording the history.

| lane | runs | solved | score | control solved | conversation solved | repo solved | $/run | memory $/run | steps | wall s | memory tool calls | history $ (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| engram | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.006 | 0.002 | 6.4 | 18 | 0.2 | 0.06 + 0.03 | – / – | – / – |
| engram+next | 40 | 40/40 | 1.00 | 8/8 | 24/24 | 8/8 | 0.006 | 0.002 | 6.5 | 18 | 0.3 | 0.06 + 0.03 | – / – | – / – |

Against the base lane (paired when the lane is a --variant):

| lane | base | solved | base solved | p (Fisher / perm) |
|---|---|---|---|---|
| engram+next | engram | 40/40 | 40/40 | 1.000 / 1.000 |

Per task (mean score):

| task | kind | engram | engram+next |
|---|---|---|---|
| count_sheet | conversation | 1.00 | 1.00 |
| stock_value | conversation | 1.00 | 1.00 |
| purchase_suggestions | conversation | 1.00 | 1.00 |
| scanner_import | conversation | 1.00 | 1.00 |
| negative_stock_advice | conversation | 1.00 | 1.00 |
| config_advice | conversation | 1.00 | 1.00 |
| last_receipt | repo | 1.00 | 1.00 |
| write_off | repo | 1.00 | 1.00 |
| abc_classes | control | 1.00 | 1.00 |
| parse_location | control | 1.00 | 1.00 |

p: two-sided. Fisher = Fisher's exact test on solved runs pooled over tasks; perm = permutation test of the mean score difference with lane labels shuffled within each task (scripts/ab_stats.py).
