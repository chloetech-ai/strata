
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 6.00 terms, 735851 postings) |
| mean AND / OR result | 2 / 686811 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.09 | 0.005 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.80 | 0.0 | 0.03 | 0.89 | 0.77 | 0.016 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.80 | 0.0 | 0.01 | 0.44 | 0.30 | 0.013 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.98 | 0.0 | 0.14 | 1.14 | 0.90 | 0.018 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 5.03 | 0.0 | 0.18 | 1.11 | 1.01 | 0.011 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.69 | 0.0 | 0.03 | 0.94 | 0.85 | 0.016 (native) | — |
| Roaring | 11.72 | 9.43 | 0.0 | 0.08 | 1.33 | 0.96 | 0.007 (native) | — |
| Trie | 9.61 | 7.28 | 0.0 | 0.39 | 10.67 | 8.06 | 0.032 (native) | — |
| Run-Trie | 9.13 | 6.78 | 0.0 | 0.43 | 11.54 | 8.81 | 0.033 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.43 | 0.0 | 0.16 | 5.63 | 5.27 | 0.026 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: PEF (5.03 bits, 0.011 ms) → Roaring (9.43 bits, 0.007 ms) → Uncompressed (32.00 bits, 0.005 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.311 |
| Uncompressed | AND | SvS + merge (branchless) | 0.385 |
| Uncompressed | AND | SvS + galloping | 0.008 |
| Uncompressed | AND | SvS + binary search | 0.048 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.012 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.228 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.196 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.005 |
| Uncompressed | AND | Small Adaptive | 0.017 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.045 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.007 |
| Uncompressed | AND | round-robin (LFTJ) | 0.044 |
| Uncompressed | AND | decode + SvS | 0.058 |
| BP128 | AND | leapfrog (NextGEQ) | 0.016 |
| BP128 | AND | round-robin (LFTJ) | 0.072 |
| BP128 | AND | decode + SvS | 0.562 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.013 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.045 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.217 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.018 |
| OptPFor | AND | round-robin (LFTJ) | 0.074 |
| OptPFor | AND | decode + SvS | 0.640 |
| PEF | AND | leapfrog (NextGEQ) | 0.011 |
| PEF | AND | round-robin (LFTJ) | 0.037 |
| PEF | AND | decode + SvS | 0.721 |
| Hybrid (bitmap + BP128) | AND | native | 0.016 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.016 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.060 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.712 |
| Roaring | AND | native | 0.007 |
| Roaring | AND | native, count only | 0.007 |
| Roaring | AND | leapfrog (NextGEQ) | 0.008 |
| Roaring | AND | round-robin (LFTJ) | 0.026 |
| Roaring | AND | decode + SvS | 0.407 |
| Trie | AND | native | 0.032 |
| Trie | AND | leapfrog (NextGEQ) | 0.060 |
| Trie | AND | round-robin (LFTJ) | 0.170 |
| Trie | AND | decode + SvS | 4.177 |
| Run-Trie | AND | native | 0.033 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.062 |
| Run-Trie | AND | round-robin (LFTJ) | 0.176 |
| Run-Trie | AND | decode + SvS | 4.493 |
| Roaring (roaring crate) | AND | native | 0.034 |
| Roaring (roaring crate) | AND | native, count only | 0.033 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.026 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.057 |
| Roaring (roaring crate) | AND | decode + SvS | 3.152 |
