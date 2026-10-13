
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 2.00 terms, 401527 postings) |
| mean AND / OR result | 743 / 400784 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.08 | 0.037 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.35 | 0.0 | 0.03 | 0.89 | 0.76 | 0.097 (round-robin (LFTJ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.35 | 0.0 | 0.01 | 0.44 | 0.29 | 0.075 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.71 | 0.0 | 0.14 | 1.13 | 0.87 | 0.101 (round-robin (LFTJ)) | — |
| PEF | 7.04 | 4.62 | 0.0 | 0.18 | 1.11 | 0.99 | 0.046 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.22 | 0.0 | 0.03 | 0.94 | 0.84 | 0.058 (native) | — |
| Roaring | 11.72 | 9.07 | 0.0 | 0.08 | 1.35 | 0.86 | 0.019 (native) | — |
| Trie | 9.61 | 6.74 | 0.0 | 0.39 | 10.59 | 7.42 | 0.145 (native) | — |
| Run-Trie | 9.13 | 6.23 | 0.0 | 0.43 | 11.54 | 8.08 | 0.154 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.07 | 0.0 | 0.16 | 5.64 | 5.17 | 0.034 (native) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: PEF (4.62 bits, 0.046 ms) → Roaring (9.07 bits, 0.019 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.357 |
| Uncompressed | AND | SvS + merge (branchless) | 0.563 |
| Uncompressed | AND | SvS + galloping | 0.039 |
| Uncompressed | AND | SvS + binary search | 0.219 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.059 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.257 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.194 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.037 |
| Uncompressed | AND | Small Adaptive | 0.053 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.048 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.044 |
| Uncompressed | AND | round-robin (LFTJ) | 0.046 |
| Uncompressed | AND | decode + SvS | 0.058 |
| BP128 | AND | leapfrog (NextGEQ) | 0.097 |
| BP128 | AND | round-robin (LFTJ) | 0.097 |
| BP128 | AND | decode + SvS | 0.338 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.075 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.075 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.150 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.101 |
| OptPFor | AND | round-robin (LFTJ) | 0.101 |
| OptPFor | AND | decode + SvS | 0.380 |
| PEF | AND | leapfrog (NextGEQ) | 0.046 |
| PEF | AND | round-robin (LFTJ) | 0.048 |
| PEF | AND | decode + SvS | 0.421 |
| Hybrid (bitmap + BP128) | AND | native | 0.058 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.066 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.068 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.430 |
| Roaring | AND | native | 0.019 |
| Roaring | AND | native, count only | 0.019 |
| Roaring | AND | leapfrog (NextGEQ) | 0.030 |
| Roaring | AND | round-robin (LFTJ) | 0.031 |
| Roaring | AND | decode + SvS | 0.202 |
| Trie | AND | native | 0.145 |
| Trie | AND | leapfrog (NextGEQ) | 0.226 |
| Trie | AND | round-robin (LFTJ) | 0.232 |
| Trie | AND | decode + SvS | 1.886 |
| Run-Trie | AND | native | 0.154 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.241 |
| Run-Trie | AND | round-robin (LFTJ) | 0.241 |
| Run-Trie | AND | decode + SvS | 2.012 |
| Roaring (roaring crate) | AND | native | 0.034 |
| Roaring (roaring crate) | AND | native, count only | 0.030 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.066 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.067 |
| Roaring (roaring crate) | AND | decode + SvS | 1.718 |
