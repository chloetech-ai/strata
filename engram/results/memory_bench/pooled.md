# Memory bench: bakehouse, stockroom, slotwise, kestrel pooled

Solved = score 1.0. p: two-sided; Fisher = Fisher's exact test on solved runs; perm = permutation test of the solved-rate difference with lane labels shuffled within each task (20,000 shuffles).

| lane | runs | solved | needs earlier sessions: solved | $/run (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|
| none | 208 | 92/208 (44%) | 40/156 | 0.005 + 0.000 | – / – | 5.8e-30 / 5e-05 |
| ccmem | 208 | 154/208 (74%) | 102/156 | 0.006 + 0.000 | 8.5e-10 / 5e-05 | 4.1e-08 / 5e-05 |
| transcripts | 208 | 162/208 (78%) | 110/156 | 0.006 + 0.000 | 2.2e-12 / 5e-05 | 4.2e-06 / 5e-05 |
| claude-mem | 208 | 125/208 (60%) | 73/156 | 0.007 + 0.008 | 0.0017 / 5e-05 | 5.9e-17 / 5e-05 |
| mem0 | 208 | 178/208 (86%) | 126/156 | 0.006 + 0.000 | 3.9e-19 / 5e-05 | 0.0093 / 5e-05 |
| engram@main | 208 | 195/208 (94%) | 143/156 | 0.006 + 0.000 | 5.8e-30 / 5e-05 | – / – |
| engram+v7 | 208 | 196/208 (94%) | 144/156 | 0.007 + 0.002 | 8.6e-31 / 5e-05 | 1 / 1 |
| engram+v5 | 208 | 201/208 (97%) | 149/156 | 0.007 + 0.002 | 1.7e-35 / 5e-05 | 0.25 / 0.016 |
| engram+rank | 208 | 201/208 (97%) | 149/156 | 0.007 + 0.001 | 1.7e-35 / 5e-05 | 0.25 / 0.033 |
| full | 208 | 194/208 (93%) | 142/156 | 0.032 + 0.000 | 3.7e-29 / 5e-05 | 1 / 1 |
| engram+nodeep | 150 | 137/150 (91%) | 98/111 | 0.006 + 0.000 | 3.1e-23 / 5e-05 | 0.54 / 0.16 |
| engram | 150 | 144/150 (96%) | 105/111 | 0.007 + 0.003 | 1.3e-29 / 5e-05 | 0.35 / 0.016 |
| engram+deep | 150 | 139/150 (93%) | 100/111 | 0.006 + 0.003 | 9.3e-25 / 5e-05 | 0.83 / 0.68 |
