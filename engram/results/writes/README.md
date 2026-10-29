# Appends while the store flushes and merges

What changed (branch `claude/project-thread-hq5p4t-engram-merges`): engram's
writer thread used to flush a full memtable into a segment and then merge
segments itself, between transactions, so every write queued behind it waited
for the flush and for whatever merge followed. Now `Store::open` starts
Strata's flusher and merger threads (`Db::spawn_flusher`, `Db::spawn_merger`
with one merge thread), the writer only commits, and a write waits for a flush
only once the memtable holds twice `flush_docs` before the flush under way
ends. The memtable now flushes at `flush_docs` records (it was four times
that, flushed by the writer at `flush_docs`).

## Method

`engram-bench writes --records 300000 --clients 1,4`: one or four client
threads append a tool call and its result (two records, `tool_drafts`) back to
back to a fresh store, 150,000 appends in all, with the `hash:256` embedder and
`StoreOptions::default()`. Each append's time is the `Store::append` call,
embedding included. `max_ms` is the longest of the 150,000; `segments` is the
count when the last append returned.

Both sides run the same `engram-bench`; only `store.rs` differs. Runs went one
at a time, alternating sides, two per side; ranges below are over the two
runs. `machine.txt` has the machine, the commits and the load averages.

## Results

Cloud container (Intel Xeon 2.30 GHz, 4 cores, 15 GiB), main → branch.

| clients | appends/s | p50 ms | p99 ms | p99.9 ms | max ms | appends ≥ 100 ms | appends ≥ 1 s | segments |
|---|---|---|---|---|---|---|---|---|
| 1 | 540–548 → 601–611 | 1.61–1.64 → 1.65–1.68 | 2.87–3.04 → 3.14–3.20 | 5.2–6.0 → 10.2–10.9 | 11,579–12,159 → 198–371 | 73 → 1 | 9 → 0 | 7 → 7 |
| 4 | 1,007–1,033 → 1,175–1,198 | 2.73–2.74 → 3.32–3.37 | 5.73–5.86 → 6.71–6.87 | 188.4–188.7 → 25.5–25.7 | 12,001–14,569 → 294–304 | 292 → 6–8 | 36 → 0 | 7 → 6–13 |

- On main, 73 appends (one client) or 292 (four) took 100 ms or more, and
  9 or 36 took over a second, up to 12–15 s: the writer was flushing or
  merging. On the branch none took a second; the longest took 0.2–0.4 s.
- Appends per second rose 11% (one client) and 16% (four), comparing the
  middles of the ranges, since the writer no longer stops to flush and merge.
- The cost is in the middle: the median and p99 rose 2–7% with one client
  and 17–22% with four, and p99.9 with one client went from 5–6 ms to
  10–11 ms. Flushes and merges now share the 4 cores with the clients while
  appends go on. With four clients p99.9 fell from 188 ms to 26 ms.
- What the branch's slowest appends (0.2–0.4 s) waited for was not
  measured.
