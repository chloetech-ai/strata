# Memory bench: what memory costs a run

Runs that logged where memory reached the agent: the fourth repeat of every task, and all four in the lanes added with it. Memory context = characters that hooks injected, memory tools returned, or the agent read from the memory's files, per run (mean). Agent input = tokens the agent read per run, cached or not (median). Memory model tokens = the memory system's own model calls per run (input + output, mean). For `full` the history is the forked session's own context, so it shows in agent input, not memory context. Wall time includes load from the lanes running beside it.

| lane | runs | solved | memory context, chars | agent input, tokens (p50) | memory model tokens | $ per run, agent + memory | wall s p50 / p95 |
|---|---|---|---|---|---|---|---|
| none | 58 | 28/58 (48%) | 0 | 124,500 | 0 | 0.0048 + 0.0000 | 24 / 46 |
| ccmem | 58 | 45/58 (78%) | 1,084 | 148,317 | 0 | 0.0054 + 0.0000 | 27 / 52 |
| transcripts | 58 | 46/58 (79%) | 4,024 | 141,318 | 0 | 0.0060 + 0.0000 | 25 / 62 |
| claude-mem | 58 | 39/58 (67%) | 17,040 | 170,823 | 145,607 | 0.0067 + 0.0082 | 36 / 71 |
| mem0 | 58 | 52/58 (90%) | 3,151 | 158,207 | 3,991 | 0.0058 + 0.0003 | 37 / 74 |
| engram@main | 58 | 54/58 (93%) | 9,770 | 141,692 | 0 | 0.0056 + 0.0000 | 28 / 47 |
| engram+v7 | 208 | 196/208 (94%) | 12,504 | 158,526 | 10,628 | 0.0068 + 0.0019 | 25 / 49 |
| engram+v5 | 208 | 201/208 (97%) | 12,401 | 151,820 | 10,341 | 0.0068 + 0.0019 | 26 / 52 |
| engram+rank | 208 | 201/208 (97%) | 12,344 | 144,022 | 7,510 | 0.0066 + 0.0013 | 25 / 51 |
| full | 58 | 55/58 (95%) | 0 | 178,930 | 0 | 0.0712 + 0.0000 | 19 / 39 |
