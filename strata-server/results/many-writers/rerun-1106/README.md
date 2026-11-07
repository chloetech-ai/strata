# Many clients writing to one server, again on 6 November 2026

The same benchmark as [the first run](../README.md) (`writers.py`, 8 seconds
per workload and client count, 4 client processes), run again after the 30
pull requests merged since #273. None of them changes how the server groups
writes into commits.

"then" is #273's head (9b3ed9b; its server code is that of 234054e, which the
first run measured), "now" is main at b022e39. Both built with
`cargo build --release`, run alternately (then, now, then, now) on one cloud
container (`machine.txt`: 4 cores shared by the server and the clients).

Requests per second, p50 / p99 latency in ms, requests per commit; the last
column is the sum of now's two runs over the sum of then's.

| workload | clients | then r1 | then r2 | now r1 | now r2 | now / then |
|---|---|---|---|---|---|---|
| SQL INSERT | 1 | 689 (1.4 / 3.2) | 644 (1.5 / 3.5) | 733 (1.3 / 3.0) | 761 (1.2 / 2.7) | 1.12 |
|  | 4 | 2,445 (1.5 / 4.0), 1.6 | 2,441 (1.5 / 3.7), 1.6 | 2,518 (1.4 / 4.2), 1.6 | 2,640 (1.4 / 3.2), 1.6 | 1.06 |
|  | 16 | 5,073 (2.9 / 8.4), 3.8 | 5,207 (2.7 / 8.2), 3.8 | 5,232 (2.7 / 8.5), 3.9 | 5,497 (2.6 / 8.0), 3.8 | 1.04 |
|  | 64 | 6,660 (8.9 / 21.2), 5.5 | 6,147 (9.6 / 23.6), 5.4 | 6,870 (8.5 / 20.5), 5.6 | 7,263 (8.1 / 19.0), 6.4 | 1.10 |
| SQL UPDATE by key | 1 | 560 (1.7 / 3.4) | 565 (1.7 / 3.1) | 625 (1.5 / 2.6) | 645 (1.5 / 2.4) | 1.13 |
|  | 4 | 2,171 (1.7 / 3.2), 1.6 | 2,136 (1.8 / 3.6), 1.6 | 2,204 (1.7 / 3.5), 1.6 | 2,276 (1.7 / 3.1), 1.6 | 1.04 |
|  | 16 | 4,587 (3.3 / 7.3), 4.2 | 4,600 (3.3 / 6.9), 4.2 | 4,595 (3.3 / 6.9), 4.5 | 5,021 (3.0 / 6.4), 4.4 | 1.05 |
|  | 64 | 6,643 (9.1 / 18.9), 6.8 | 6,040 (10.0 / 20.7), 6.9 | 6,722 (8.9 / 19.3), 8.2 | 6,750 (9.0 / 18.4), 7.0 | 1.06 |
| SQL UPDATE, one row | 1 | 577 (1.6 / 3.1) | 529 (1.8 / 3.5) | 567 (1.7 / 3.2) | 577 (1.7 / 3.2) | 1.03 |
|  | 4 | 1,844 (2.0 / 4.7), 1.0 | 1,843 (2.1 / 4.5), 1.0 | 2,011 (1.9 / 3.9), 1.0 | 2,058 (1.8 / 3.4), 1.0 | 1.10 |
|  | 16 | 1,841 (8.4 / 14.2), 1.0 | 1,778 (8.8 / 14.5), 1.0 | 2,017 (7.7 / 12.7), 1.0 | 2,001 (7.8 / 12.5), 1.0 | 1.11 |
|  | 64 | 1,953 (32.0 / 45.0), 1.0 | 1,802 (35.2 / 45.1), 1.0 | 2,057 (30.8 / 41.2), 1.0 | 1,912 (32.8 / 46.8), 1.0 | 1.06 |
| /v1/write insert | 1 | 1,035 (0.9 / 2.1) | 992 (0.9 / 2.2) | 1,082 (0.9 / 1.9) | 1,122 (0.8 / 1.9) | 1.09 |
|  | 4 | 3,489 (1.0 / 2.7), 1.7 | 3,299 (1.1 / 3.3), 1.7 | 3,166 (1.1 / 4.0), 1.8 | 3,590 (1.0 / 2.9), 1.7 | 1.00 |
|  | 16 | 7,150 (2.0 / 6.7), 5.2 | 6,659 (2.1 / 7.3), 5.3 | 7,249 (1.9 / 7.2), 5.3 | 7,792 (1.8 / 6.6), 5.3 | 1.09 |
|  | 64 | 9,078 (3.7 / 15.7), 6.5 | 9,260 (4.2 / 15.9), 7.3 | 10,752 (3.5 / 17.0), 7.3 | 9,452 (3.3 / 16.2), 6.3 | 1.10 |
| /v1/write add | 1 | 918 (1.0 / 2.6) | 905 (1.0 / 2.2) | 951 (1.0 / 2.1) | 1,067 (0.9 / 1.9) | 1.11 |
|  | 4 | 3,168 (1.2 / 2.5), 1.7 | 3,259 (1.1 / 2.4), 1.7 | 3,613 (1.0 / 2.2), 1.8 | 3,260 (1.1 / 2.6), 1.8 | 1.07 |
|  | 16 | 7,136 (2.1 / 5.0), 5.4 | 7,226 (2.1 / 4.8), 5.2 | 7,922 (1.9 / 4.4), 5.5 | 7,880 (1.9 / 4.5), 5.4 | 1.10 |
|  | 64 | 9,961 (3.6 / 12.7), 6.0 | 9,002 (2.9 / 10.8), 5.5 | 8,678 (2.3 / 8.8), 5.3 | 9,130 (2.9 / 13.5), 5.7 | 0.94 |

- Main is 0 to 13% faster than #273's head on every workload but one
  (`/v1/write add` at 64 clients, 6% slower, inside the two runs' own spread:
  then's runs differ by 10%). The grouping is unchanged: as many requests
  share a commit as before.
- The counters add up in all four runs (331,658, 318,170, 336,439 and
  341,349 adds and UPDATEs answered), with no request refused or failed.
- This container is not the first run's (2.1 GHz here, 2.8 GHz there), and
  #273's head runs faster on it than in the first run at 16 and 64 clients
  (SQL INSERT at 64: 6,147 to 6,660 here, 4,903 to 5,496 there), which is
  why both sides ran again here rather than comparing with those numbers.
