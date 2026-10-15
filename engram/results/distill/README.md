# Note-writer test set

`eval/distill/cases.jsonl` holds 12 exchanges, each with what a correct set of
notes must and must not contain (checked by `scripts/distill_eval.py`).
Each case was distilled five times with Haiku (`engram-bench distill
--repeats 5`), in the cloud container, in a clean environment.

| prompt | outputs passing | cost |
|---|---|---|
| before (`before/`) | 54 / 60 | $0.035 |
| after (`after/`): the assistant's own habits and stale status left out more firmly | 58 / 60 | $0.032 |

Remaining failures after the change: one note that the user turned down
Redis caching, which the strict grader counts as noting the suggestion, and
one note naming the branch work was pushed to.
