# Memory bench: bakehouse, stockroom, slotwise, kestrel pooled

Solved = score 1.0. p: two-sided; Fisher = Fisher's exact test on solved runs; perm = permutation test of the solved-rate difference with lane labels shuffled within each task (20,000 shuffles).

| lane | runs | solved | needs earlier sessions: solved | $/run (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |
|---|---|---|---|---|---|---|
| none | 150 | 64/150 (43%) | 25/111 | 0.005 + 0.000 | – / – | 3e-23 / 5e-05 |
| ccmem | 150 | 109/150 (73%) | 70/111 | 0.006 + 0.000 | 2.2e-07 / 5e-05 | 7.5e-07 / 5e-05 |
| transcripts | 150 | 116/150 (77%) | 77/111 | 0.006 + 0.000 | 1.2e-09 / 5e-05 | 5.2e-05 / 5e-05 |
| claude-mem | 150 | 86/150 (57%) | 47/111 | 0.007 + 0.008 | 0.015 / 5e-05 | 3.2e-14 / 5e-05 |
| mem0 | 150 | 126/150 (84%) | 87/111 | 0.006 + 0.000 | 8.4e-14 / 5e-05 | 0.0089 / 5e-05 |
| engram@main | 150 | 141/150 (94%) | 102/111 | 0.006 + 0.000 | 3e-23 / 5e-05 | – / – |
| engram+nodeep | 150 | 137/150 (91%) | 98/111 | 0.006 + 0.000 | 3.5e-20 / 5e-05 | 0.51 / 0.099 |
| engram | 150 | 144/150 (96%) | 105/111 | 0.007 + 0.003 | 5.4e-26 / 5e-05 | 0.6 / 0.099 |
| engram+deep | 150 | 139/150 (93%) | 100/111 | 0.006 + 0.003 | 1.2e-21 / 5e-05 | 0.82 / 0.66 |
| full | 150 | 139/150 (93%) | 100/111 | 0.016 + 0.000 | 1.2e-21 / 5e-05 | 0.82 / 0.6 |
