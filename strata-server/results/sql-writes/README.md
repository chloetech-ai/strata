# SQL writes with an embedded field: model outside the writer lock

`bench_sqlw.py` starts strata-server on a fresh database whose `emb` field is
bge-small-en-v1.5's embedding of `title` and `body` (the `bert` feature, on
the CPU), and sends SQL over HTTP:

- `inserts_1`, `inserts_4`: 1 client inserting 200 rows, then 4 clients
  inserting 100 each, back to back; every row is embedded (8 + 60 words).
- `updates_alone`: one client sends `UPDATE docs SET stock = stock + 1 WHERE
  id = :k` (in place, nothing to embed) for 10 s.
- `updates_beside_2_inserting`: the same for 20 s while 2 clients insert
  rows.

main (299465e) ran SQL writes on the server's writer thread, and `Db::sql`
computed a row's vector inside the write transaction, so every other write
waited for the model. The branch prepares a SQL write (binding and its
vectors) on a blocking thread before it queues for the writer
(`Db::prepare_write`, `Db::write_prepared`).

Two runs per side, alternating, on the cloud container (machine.txt). Rates
are per second; latencies in ms.

| | main r1 | main r2 | branch r1 | branch r2 |
|---|---|---|---|---|
| inserts, 1 client | 9.9 (p50 100) | 10.2 (p50 97) | 10.2 (p50 98) | 10.2 (p50 97) |
| inserts, 4 clients | 9.9 (p50 402) | 9.9 (p50 400) | 23.1 (p50 171) | 23.5 (p50 169) |
| updates alone | 1,317 (p50 0.73, p99 1.27) | 1,262 (p50 0.75, p99 1.39) | 1,215 (p50 0.78, p99 1.41) | 1,234 (p50 0.76, p99 1.39) |
| updates beside 2 inserting clients | 4.7 (p50 214, p99 280) | 4.8 (p50 210, p99 249) | 500 (p50 1.3, p99 13.4) | 516 (p50 1.3, p99 13.3) |
| inserts meanwhile | 9.8 | 10.1 | 15.4 | 15.0 |

- Beside rows being embedded, an in-place `UPDATE` no longer waits for the
  model: 104 to 110 times the rate, and its median latency falls from
  210 ms to 1.3 ms.
- Inserts from 4 clients go 2.3 to 2.4 times as fast: their vectors are computed on
  as many writes at once as there are cores, instead of one at a time under
  the lock. One client's inserts are unchanged.
- Alone, in-place updates run 2 to 8% slower (median 0.01 to 0.05 ms more):
  every SQL write is now prepared on a blocking thread before it queues.

Rerun: build strata-server with `--features bert` at each commit, put
bge-small-en-v1.5 (`config.json`, `tokenizer.json`, `model.safetensors`)
in `MODELS/bge-small-en-v1.5`, then
`python3 bench_sqlw.py SERVER_BIN MODELS OUT.json`.
