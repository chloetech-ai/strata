# The session tail on the memory bench

Does per-prompt recall improve when it also searches with what the session
was just doing (`ENGRAM_SESSION_CONTEXT=1`)? The tail is the session's last
20 records: the files they name and the first line of their errors become a
BM25 query of at most 24 words, and the vectors of the newest 4 prompts,
answers, tool calls and errors rank stored records by `maxsim`. A record's
best normalised score in those two lists is added to its recall score with
weight 0.5 (beside BM25's and cosine's, which sum to 1), and records about one
of the tail's files score 15% more. The gate still decides from the prompt's
own evidence, so the tail reorders what is injected but not whether anything
is.

## Method

`scripts/memory_bench.py` on its four projects, run in the cloud container
(Intel Xeon at 2.10 GHz, 4 cores, Linux) on 2026-10-30, with Haiku for the
agent and for Engram's own calls (the query engine on, in the background, as
by default):

- Each project's history was recorded once with Engram, tail off
  (`history_engram.json`; build a70a4ef, whose recall without the tail is the
  same as the final build's).
- Each follow-up task then ran four times in lane `engram` (tail off) and four
  times in its `--variant` lane `engram+tail`
  (`ENGRAM_SESSION_CONTEXT=1`), both from the same frozen history, so the two
  lanes differ only in the variable (build 11b5c43).
- A session's tail is empty on its first prompt, so every run opened with the
  same first prompt (`--lead-in`): "Before we start: run the test suite and
  tell me in two or three sentences what this project does and where its main
  code lives. Don't change anything yet." The task was the session's second
  prompt, with that exploration as its tail.

```bash
python3 scripts/memory_bench.py all --scenario kestrel --lanes engram --repeats 4 \
  --variant engram+tail=ENGRAM_SESSION_CONTEXT=1 --lead-in "Before we start: …"
python3 results/memory_bench_tail/pool.py
```

## Results

| project | engram | engram+tail | tasks that differ |
|---|---|---|---|
| bakehouse | 68/72 | 69/72 | `vat_export` 3 → 4 of 4 |
| kestrel | 52/56 | 50/56 | `free_slots` 4 → 2 of 4 (the 15-minute buffer after an assessment) |
| slotwise | 39/40 | 40/40 | `sms_reminder` 3 → 4 of 4 |
| stockroom | 40/40 | 39/40 | `negative_stock_advice` 4 → 3 of 4 |
| **all** | **199/208** | **198/208** | |

Pooled over the 52 tasks (`pooled.json`): Fisher's exact test on solved runs
p = 1.0; a permutation test with lane labels shuffled within each task gives
p = 0.50 for the mean score (+0.005 with the tail) and p = 1.0 for solved runs.
Bakehouse's tasks that imply a requirement without naming it: 13 of 16 with
the tail, 12 without.

| per run | engram | engram+tail |
|---|---|---|
| agent + memory, USD | 0.0091 | 0.0093 |
| Engram's own model calls, USD | 0.0038 | 0.0038 |
| characters the prompt hook injected | 4,051 | 4,086 |

The tail made no measurable difference: one run fewer of 208, four tasks that
moved by one or two runs in either direction, and a gate that decided alike
(as many skipped, brief and full injections in each lane). It
stays off by default. These tasks mostly need a fact said in an earlier
session that the prompt itself names; a tail would matter more where the
prompt is vague ("fix this") and the work in progress says what it is about,
which this bench does not test.

What it costs a prompt is in `../tail/tail_static.json` (`engram-bench tail`,
same container): the prompt's searches take 2.85 → 3.52 ms at the median at
100,000 records and 10.5 → 16.3 ms at 1,000,000, almost all of it `maxsim`'s
four nearest-neighbour searches.

`<project>/runs.jsonl` has every run, `<project>/summary.md` the per-task
table, and `<project>/patches.tar.gz` each run's diff of the repository
(`tar -xzf <project>/patches.tar.gz -C <project>`).
