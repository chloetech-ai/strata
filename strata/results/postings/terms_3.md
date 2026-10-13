
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 3.00 terms, 684896 postings) |
| mean AND / OR result | 111 / 669995 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.08 | 0.021 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.51 | 0.0 | 0.03 | 0.89 | 0.76 | 0.049 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.51 | 0.0 | 0.01 | 0.44 | 0.30 | 0.039 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.81 | 0.0 | 0.14 | 1.12 | 0.86 | 0.051 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.77 | 0.0 | 0.18 | 1.12 | 1.00 | 0.029 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.39 | 0.0 | 0.03 | 0.94 | 0.86 | 0.046 (leapfrog (NextGEQ)) | — |
| Roaring | 11.72 | 9.17 | 0.0 | 0.08 | 1.34 | 0.90 | 0.017 (native) | — |
| Trie | 9.61 | 6.96 | 0.0 | 0.39 | 10.54 | 7.67 | 0.084 (native) | — |
| Run-Trie | 9.13 | 6.46 | 0.0 | 0.43 | 11.49 | 8.35 | 0.088 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.17 | 0.0 | 0.16 | 5.58 | 5.17 | 0.049 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: PEF (4.77 bits, 0.029 ms) → Roaring (9.17 bits, 0.017 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.498 |
| Uncompressed | AND | SvS + merge (branchless) | 0.679 |
| Uncompressed | AND | SvS + galloping | 0.023 |
| Uncompressed | AND | SvS + binary search | 0.132 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.035 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.345 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.278 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.021 |
| Uncompressed | AND | Small Adaptive | 0.033 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.052 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.023 |
| Uncompressed | AND | round-robin (LFTJ) | 0.048 |
| Uncompressed | AND | decode + SvS | 0.064 |
| BP128 | AND | leapfrog (NextGEQ) | 0.049 |
| BP128 | AND | round-robin (LFTJ) | 0.102 |
| BP128 | AND | decode + SvS | 0.537 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.039 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.070 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.216 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.051 |
| OptPFor | AND | round-robin (LFTJ) | 0.108 |
| OptPFor | AND | decode + SvS | 0.603 |
| PEF | AND | leapfrog (NextGEQ) | 0.029 |
| PEF | AND | round-robin (LFTJ) | 0.048 |
| PEF | AND | decode + SvS | 0.680 |
| Hybrid (bitmap + BP128) | AND | native | 0.047 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.046 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.068 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.694 |
| Roaring | AND | native | 0.017 |
| Roaring | AND | native, count only | 0.017 |
| Roaring | AND | leapfrog (NextGEQ) | 0.020 |
| Roaring | AND | round-robin (LFTJ) | 0.030 |
| Roaring | AND | decode + SvS | 0.324 |
| Trie | AND | native | 0.084 |
| Trie | AND | leapfrog (NextGEQ) | 0.150 |
| Trie | AND | round-robin (LFTJ) | 0.237 |
| Trie | AND | decode + SvS | 3.343 |
| Run-Trie | AND | native | 0.088 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.157 |
| Run-Trie | AND | round-robin (LFTJ) | 0.255 |
| Run-Trie | AND | decode + SvS | 3.608 |
| Roaring (roaring crate) | AND | native | 0.058 |
| Roaring (roaring crate) | AND | native, count only | 0.057 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.049 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.065 |
| Roaring (roaring crate) | AND | decode + SvS | 2.899 |
