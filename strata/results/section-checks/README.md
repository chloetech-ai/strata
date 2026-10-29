# What checking each section on its first read costs

Each section of a segment has a crc32, and with `DbOptions::check_sections`
(on by default) a query checks a section against it the first time it reads
it, which reads the whole section once. `examples/section_checks.rs` measures
what that costs on one database:

- 1,000,000 documents in 10 segments of 100,000 (687 MB): a keyword key, an
  integer `n`, 30 words of text with positions from a 20,000-word vocabulary
  (word *i* about as often as 1 / (*i* + 1)), and a 64-dimensional HNSW vector
  (`section_checks PATH make 1000000 64`). Section sizes: `sizes.tsv`.
- Each run opens the database in a fresh `Db` and runs five queries twice:
  `filter` (a count over a range of `n`), `text` (BM25 top 10), `phrase` (a
  count of a two-word phrase), `vector` (similarity top 10), `fetch` (one row's
  key and text by key). "Cold": the file's pages evicted from the page cache
  (`posix_fadvise(DONTNEED)`) before opening; "warm": left in it. Three
  rounds, alternating check on and off (`section_checks PATH measure 3`).

Milliseconds, the range over three rounds (`measure.jsonl`). Cloud container,
see machine.txt. The container's disk is virtual and the host may cache it,
so "cold" reads ran at 0.4 to 2.3 GB/s and vary from run to run.

| | check, cold | no check, cold | check, warm | no check, warm |
|---|---|---|---|---|
| open | 17.4-18.7 | 3.1-3.9 | 4.2-4.3 | 0.5 |
| filter, first | 2.8-3.3 | 2.2-2.6 | 0.6 | 0.4 |
| text, first | 55-284 | 9.1-9.9 | 14.6-18.5 | 1.3-1.5 |
| phrase, first | 3.0-3.7 | 5.9-6.1 | 3.0-3.3 | 3.0-3.2 |
| vector, first | 202-453 | 385-487 | 46-54 | 4.7-4.9 |
| fetch, first | 9.0-15.7 | 7.7-8.0 | 1.9-2.4 | 0.3 |
| every second run | as without the check | | | |

- Opening checks every segment's keys (35 MB here): 14 ms more cold, 4 ms
  warm.
- The first query that reads a field pays for reading its sections whole
  once: warm, +13 to 17 ms for text (69 MB of sections) and +41 to 49 ms
  for the vector (452 MB). Cold, a text query also waits for its sections to come from
  disk; a vector query is no slower, since reading its sections in long runs
  costs less than the page-at-a-time reads of an HNSW search on a cold cache
  that it then saves. The phrase query runs after the text query has
  checked the positions.
- Queries after the first cost what they cost without the check.

Before the check let the kernel read ahead (`measure_before_read_fix.jsonl`,
commit c77308b), the sections advised `MADV_RANDOM` (doc store, float32
vectors, HNSW graph, SQ8 codes, keys) were read a page per fault: cold, the
first vector query took 5,055 to 5,335 ms, opening 374 to 437 ms and the
first fetch 149 to 257 ms. `scan.tsv` is the time to check each kind of
section cold, one segment at a time (`section_checks PATH scan`).
