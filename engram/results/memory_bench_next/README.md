# Next files on the memory bench

Does it help to name, before an edit, the files that earlier sessions usually
edited next (`ENGRAM_NEXT_FILES=1`)? The daemon counts which files followed
which in the edits the store holds as tool calls: B follows A when an edit of
B came within three edits after one of A (consecutive edits of one file count
once), weighted by one over the gap and summed over sessions. Before an edit of
A, the PreToolUse hook names at most two files above weight 1 that still
exist, each with one note (a decision, rule or fact about it, else its newest
error), once per file and session.

## Method

`scripts/memory_bench.py` on its four projects, run in the cloud container
(Intel Xeon at 2.10 GHz, 4 cores, Linux) on 2026-10-31, with Haiku for the
agent and for Engram's own calls (the query engine on, in the background, as
by default), build fc4c37e:

- Each project's history was recorded once with Engram, next files off
  (`history_engram.json`).
- Each follow-up task then ran four times in lane `engram` (next files off)
  and four times in its `--variant` lane `engram+next`
  (`ENGRAM_NEXT_FILES=1`), both from the same frozen history, so the two
  lanes differ only in the variable. The counts come from the history's
  edits, and from the run's own as it goes.

```bash
python3 scripts/memory_bench.py all --scenario kestrel --lanes engram --repeats 4 \
  --variant engram+next=ENGRAM_NEXT_FILES=1
python3 results/memory_bench_next/pool.py
python3 results/memory_bench_next/edits.py <the bench's --work directory>
```

## Results

| project | engram | engram+next | tasks that differ |
|---|---|---|---|
| bakehouse | 67/72 | 65/72 | `bake_sheet` 3 → 1 of 4 (dough in whole grams; next files shown in all four runs); `van_list` 0 → 0, mean score 0.62 → 0.44 (none shown) |
| kestrel | 52/56 | 52/56 | `can_book` 0 → 0, mean score 0.40 → 0.15 (shown in one run, which scored 0.40) |
| slotwise | 40/40 | 40/40 | |
| stockroom | 40/40 | 40/40 | |
| **all** | **199/208** | **197/208** | |

Pooled over the 52 tasks (`pooled.json`): Fisher's exact test on solved runs
p = 0.82; a permutation test with lane labels shuffled within each task gives
p = 0.055 for the mean score (−0.012 with next files) and p = 0.49 for solved
runs. Of that score difference, `van_list` and `can_book` account for about
three quarters, in runs that were shown no next files.

Next files were shown in 65 of the 208 runs, in 18 of the 52 tasks (130 times,
naming 207 files); in those 18 tasks 65 of 72 runs were solved with them and
67 without.

Were they the files edited next? (`edits.json`, from the runs' transcripts.)
Of the 207 files named, the run went on to edit 59 (29%). In the lane without
next files, after its first edit of the same file in the same task, the run
went on to edit the file that would have been named 210 times of 720 (29%).
So the hint did not change what the agent edited, and the counts guessed the
next file right about a time in four. Of the 57 notes shown with the files, 16
were facts and 8 decisions; the other 33 were errors that name the file,
mostly shell output such as a directory listing, because no knowledge was
written about it.

| per run | engram | engram+next |
|---|---|---|
| agent + memory, USD | 0.0068 | 0.0067 |
| Engram's own model calls, USD | 0.0020 | 0.0019 |
| characters the edit hook injected | 525 | 755 |
| characters the prompt hook injected | 3,331 | 3,331 |

Recording the histories cost $0.21 + $0.12 (bakehouse), $0.15 + $0.08
(kestrel), $0.06 + $0.03 (slotwise) and $0.06 + $0.03 (stockroom), agent and
Engram's own calls.

Next files made no measurable difference, and they stay off by default. These
tasks say which files to change, or the agent finds them by reading the code
first; a hint of what usually comes next would matter more in a repository
too large to read, where a change in one place needs a change in another that
nothing names.

`<project>/runs.jsonl` has every run, `<project>/summary.md` the per-task
table, and `<project>/patches.tar.gz` each run's diff of the repository
(`tar -xzf <project>/patches.tar.gz -C <project>`).
