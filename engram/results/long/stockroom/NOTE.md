# Clean long-session A/B run: hand review of the advice answers

This run (and ../slotwise) used the isolated harness: Claude Code's auto
memory is reset before each lane's history and restored before every task,
and only the repository's own settings and MCP servers load. There were no
errors.

runs.jsonl holds the graders' raw output. The advice tasks are graded by
regular expressions over the answer, so every unsolved advice answer and
every pass of a fact check was read by hand. Two grades were wrong:

- compact, config_advice, repeat 0: graded 1.0, should be 0.5. The answer
  says the project "has to run on 3.8" and then asks "Is 3.8 still a real
  target? If the shops' machines are on 3.11 or later...". It does not know
  that the back-office PC is stuck on 3.8; the check matched on "machine".
- compact+engram, negative_stock_advice, repeat 1: graded 0.0, should be
  0.5. "I'd leave `issue()` alone" advises against raising, but the pattern
  `leave (it|issue)` does not allow the backtick. The answer still gives an
  invented reason instead of the till posting after payment, so it stays
  unsolved.

After the hand review:

| lane | solved | mean score |
|---|---|---|
| compact | 26/30 (86.7%) | 0.933 |
| compact+engram | 27/30 (90.0%) | 0.950 |
| compact+engram+graph | 30/30 (100%) | 1.000 |
| full | 30/30 (100%) | 1.000 |

Each compacted lane lost one fact that was only ever said in conversation,
and in each case it is the fact its own last compaction summary dropped:

- compact's last summary quotes the till rule but keeps only "keep Python
  3.8 compatibility", without the back-office PC. It missed the PC in all
  three config_advice repeats.
- compact+engram's last summary keeps the back-office PC but only "issue()
  must never refuse", without the till. It missed why in all three
  negative_stock_advice repeats, giving no reason or a made-up one.
- compact+engram+graph's last summary keeps both facts, and it answered both.

Engram added nothing to either advice question. Its recall gate decided
"Skip" on both prompts in both Engram lanes (the last entry of `decisions`
in each run's `engram` field), and no advice run called a memory tool. So this run
does not show Engram recovering a fact compaction dropped; the differences
between the compacted lanes come from their summaries.

Engram lanes wrote no auto-memory notes during their histories; the
non-Engram lanes did (`claude_memory_files` in each history file).

Slotwise: every lane solved every task. The ical_advice passes were read by
hand and all are genuine.
