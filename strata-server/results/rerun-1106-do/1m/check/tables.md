## Load time and index size

| engine | load (s) | of which: send / build | searchable | merge to 1 segment | index size (GB) | documents |
|---|---|---|---|---|---|---|

## 1 client: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 532 | 1.64 / 2.68 / 5.46 |
| bm25 + filter | 458 | 1.90 / 3.22 / 6.87 |
| phrase | 750 | 1.30 / 1.51 / 2.00 |
| typo | 79 | 12.69 / 17.80 / 24.83 |
| prefix | 482 | 1.83 / 3.15 / 4.82 |
| vector | 544 | 1.82 / 2.00 / 2.59 |
| vector + filter | 461 | 2.13 / 2.38 / 3.34 |
| hybrid (RRF) | 209 | 4.50 / 6.08 / 10.55 |
| hybrid (weighted) | 206 | 4.55 / 6.33 / 10.42 |
| boosted (bm25 × decay) | 234 | 3.60 / 7.52 / 11.69 |
| similar items | 440 | 2.25 / 2.49 / 3.36 |
| facet | 652 | 1.53 / 1.84 / 2.32 |
| key lookup | 1,392 | 0.70 / 0.79 / 1.05 |
| sql aggregate | 132 | 7.60 / 10.00 / 12.29 |


## 4 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 1,281 | 2.83 / 4.55 / 8.24 |
| bm25 + filter | 1,131 | 3.16 / 5.18 / 10.46 |
| phrase | 1,651 | 2.37 / 3.07 / 3.78 |
| typo | 205 | 19.17 / 28.99 / 39.11 |
| prefix | 1,202 | 3.03 / 4.98 / 7.40 |
| vector | 1,281 | 3.06 / 3.95 / 4.91 |
| vector + filter | 1,117 | 3.51 / 4.44 / 5.63 |
| hybrid (RRF) | 508 | 7.39 / 10.95 / 15.80 |
| hybrid (weighted) | 489 | 7.74 / 11.29 / 16.76 |
| boosted (bm25 × decay) | 640 | 5.20 / 10.72 / 19.01 |
| similar items | 1,012 | 3.84 / 5.07 / 6.22 |
| facet | 1,404 | 2.80 / 3.68 / 4.62 |
| key lookup | 3,954 | 0.98 / 1.29 / 1.64 |
| sql aggregate | 308 | 12.79 / 18.41 / 22.47 |


## 16 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 1,888 | 8.08 / 11.29 / 16.21 |
| bm25 + filter | 1,749 | 8.60 / 12.51 / 19.13 |
| phrase | 1,849 | 8.59 / 10.65 / 12.53 |
| typo | 221 | 71.89 / 109.12 / 171.53 |
| prefix | 1,771 | 8.76 / 11.71 / 15.63 |
| vector | 1,462 | 10.92 / 13.74 / 16.62 |
| vector + filter | 1,249 | 12.60 / 15.81 / 19.83 |
| hybrid (RRF) | 655 | 22.99 / 33.39 / 49.09 |
| hybrid (weighted) | 613 | 24.96 / 35.21 / 48.85 |
| boosted (bm25 × decay) | 835 | 15.65 / 34.94 / 62.31 |
| similar items | 1,121 | 13.97 / 17.86 / 22.08 |
| facet | 1,646 | 9.53 / 12.71 / 15.62 |
| key lookup | 5,509 | 2.86 / 3.69 / 4.53 |
| sql aggregate | 296 | 51.93 / 86.49 / 106.54 |


Errors during the timed runs: Strata 0

## Answers: recall@10 against an exact search, nDCG@10 against the collection's judgments

Unavailable kinds are n/a; recall is for the kinds an exact search can answer.

| kind | Strata recall | Strata nDCG |
|---|---|---|
| bm25 | – | – |
| bm25 + filter | – | – |
| phrase | – | – |
| typo | – | – |
| prefix | – | – |
| vector | 0.806 | – |
| vector + filter | 0.828 | – |
| hybrid (RRF) | – | – |
| hybrid (weighted) | – | – |
| boosted (bm25 × decay) | – | – |
| similar items | 0.990 | – |


## SQL aggregates equal to the true answer

| Strata |
|---|
| 300 of 300 |

## Agreement with Strata's answers

