## Load time and index size

| engine | load (s) | of which: send / build | searchable | merge to 1 segment | index size (GB) | documents |
|---|---|---|---|---|---|---|

## 1 client: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 236 | 2.97 / 8.01 / 22.02 |
| bm25 + filter | 203 | 3.57 / 8.88 / 23.79 |
| phrase | 560 | 1.69 / 2.18 / 3.38 |
| typo | 21 | 45.47 / 73.27 / 98.59 |
| prefix | 141 | 5.30 / 14.15 / 30.44 |
| vector | 413 | 2.39 / 2.70 / 3.44 |
| vector + filter | 312 | 3.17 / 3.69 / 4.59 |
| hybrid (RRF) | 102 | 7.97 / 15.76 / 38.39 |
| hybrid (weighted) | 100 | 8.25 / 15.62 / 37.35 |
| boosted (bm25 × decay) | 97 | 8.03 / 19.43 / 41.10 |
| similar items | 219 | 4.54 / 5.20 / 6.02 |
| facet | 365 | 2.36 / 4.37 / 6.71 |
| key lookup | 1,220 | 0.80 / 0.95 / 1.25 |
| sql aggregate | 22 | 44.30 / 71.55 / 82.04 |


## 4 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 564 | 4.88 / 13.86 / 39.78 |
| bm25 + filter | 475 | 5.98 / 15.49 / 42.98 |
| phrase | 1,291 | 2.97 / 4.07 / 5.75 |
| typo | 43 | 89.39 / 154.61 / 213.97 |
| prefix | 305 | 9.20 / 27.29 / 55.71 |
| vector | 1,032 | 3.78 / 4.83 / 6.33 |
| vector + filter | 712 | 5.39 / 7.36 / 9.45 |
| hybrid (RRF) | 238 | 13.42 / 28.54 / 81.30 |
| hybrid (weighted) | 235 | 13.69 / 27.76 / 70.44 |
| boosted (bm25 × decay) | 213 | 14.11 / 35.94 / 80.08 |
| similar items | 491 | 7.86 / 10.43 / 13.29 |
| facet | 876 | 4.07 / 7.16 / 10.21 |
| key lookup | 3,886 | 1.00 / 1.29 / 1.69 |
| sql aggregate | 32 | 122.79 / 185.42 / 217.92 |


## 16 clients: queries per second, and latency in ms (p50 / p90 / p99)

| kind | Strata q/s | Strata ms |
|---|---|---|
| bm25 | 584 | 15.10 / 61.16 / 177.11 |
| bm25 + filter | 497 | 18.47 / 70.06 / 204.06 |
| phrase | 1,516 | 10.43 / 13.50 / 16.79 |
| typo | 40 | 373.81 / 637.20 / 977.76 |
| prefix | 297 | 34.41 / 120.39 / 273.27 |
| vector | 1,231 | 12.76 / 16.34 / 20.76 |
| vector + filter | 898 | 16.35 / 26.72 / 37.95 |
| hybrid (RRF) | 239 | 51.60 / 120.70 / 322.33 |
| hybrid (weighted) | 239 | 52.22 / 122.53 / 320.04 |
| boosted (bm25 × decay) | 197 | 56.69 / 166.27 / 418.48 |
| similar items | 551 | 27.82 / 42.68 / 56.48 |
| facet | 1,094 | 14.09 / 18.95 / 25.32 |
| key lookup | 5,016 | 3.12 / 4.11 / 5.06 |
| sql aggregate | 30 | 533.44 / 790.38 / 901.48 |


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
| vector | 0.797 | – |
| vector + filter | 0.829 | – |
| hybrid (RRF) | – | – |
| hybrid (weighted) | – | – |
| boosted (bm25 × decay) | – | – |
| similar items | 0.975 | – |


## SQL aggregates equal to the true answer

| Strata |
|---|
| 300 of 300 |

## Agreement with Strata's answers

