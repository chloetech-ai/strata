# Open questions written down as requirements

In the slotwise comparison the lane with the query engine in `sync` mode failed
`cancellation_fee` in all three runs: the agent asked about boundaries and
rounding instead of writing the function. The history had stated the fee
tiers; the assistant had answered with five questions of its own, and the
distiller turned those into a requirement ("these cancellation fee details are
still undecided and must be settled before the fee is built"). The query
engine picked that note for the task, and the agent did what it said.

The distill prompt now leaves out questions the assistant asked and
assumptions it made that the user has not answered or confirmed.

`run.py` distills that one exchange (`exchange.json`, from the slotwise
history) six times with the prompt before (`old`) and after (`new`) the change
(`prompts.json`), with Haiku through `claude -p`, as Engram calls it.
`results.json` holds every output.

| prompt | outputs with an open-question requirement | outputs keeping the fee tiers as a requirement |
|---|---|---|
| before | 6 / 6 | 6 / 6 |
| after | 0 / 6 | 6 / 6 |

Cost: $0.012 for the twelve calls. Cloud VM, 2026-10-15. The comparison of
memory systems ran on the build before this change.
