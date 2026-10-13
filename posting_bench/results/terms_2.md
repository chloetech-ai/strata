
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 2.00 terms, 401527 postings) |
| mean AND / OR result | 743 / 400784 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.05 | 0.22 | 0.24 | 0.090 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.35 | 0.05 | 0.89 | 0.67 | 0.116 (round-robin (LFTJ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.35 | 0.03 | 0.66 | 0.43 | 0.108 (round-robin (LFTJ)) | — |
| OptPFor | 7.13 | 5.71 | 0.22 | 1.21 | 0.78 | 0.124 (round-robin (LFTJ)) | — |
| PEF | 7.04 | 4.62 | 0.17 | 1.62 | 1.30 | 0.054 (round-robin (LFTJ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.22 | 0.05 | 1.07 | 0.93 | 0.073 (native) | — |
| Roaring | 11.72 | 9.07 | 0.14 | 2.60 | 1.50 | 0.028 (native) | — |
| Trie | 9.61 | 6.74 | 0.60 | 7.62 | 4.54 | 0.187 (native) | — |
| Run-Trie | 9.13 | 6.23 | 0.68 | 10.03 | 6.14 | 0.167 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.07 | 0.28 | 8.34 | 7.34 | 0.054 (native) | — |

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.430 |
| Uncompressed | AND | SvS + merge (branchless) | 1.341 |
| Uncompressed | AND | SvS + galloping | 0.092 |
| Uncompressed | AND | SvS + binary search | 0.427 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.124 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.537 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.546 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.454 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.090 |
| Uncompressed | AND | Small Adaptive | 0.100 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.099 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.096 |
| Uncompressed | AND | round-robin (LFTJ) | 0.099 |
| Uncompressed | AND | decode + SvS | 0.208 |
| BP128 | AND | leapfrog (NextGEQ) | 0.122 |
| BP128 | AND | round-robin (LFTJ) | 0.116 |
| BP128 | AND | decode + SvS | 0.363 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.109 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.108 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.255 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.129 |
| OptPFor | AND | round-robin (LFTJ) | 0.124 |
| OptPFor | AND | decode + SvS | 0.391 |
| PEF | AND | leapfrog (NextGEQ) | 0.057 |
| PEF | AND | round-robin (LFTJ) | 0.054 |
| PEF | AND | decode + SvS | 0.573 |
| Hybrid (bitmap + BP128) | AND | native | 0.073 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.082 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.088 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.571 |
| Roaring | AND | native | 0.028 |
| Roaring | AND | native, count only | 0.027 |
| Roaring | AND | leapfrog (NextGEQ) | 0.044 |
| Roaring | AND | round-robin (LFTJ) | 0.050 |
| Roaring | AND | decode + SvS | 0.296 |
| Trie | AND | native | 0.187 |
| Trie | AND | leapfrog (NextGEQ) | 0.224 |
| Trie | AND | round-robin (LFTJ) | 0.230 |
| Trie | AND | decode + SvS | 0.919 |
| Run-Trie | AND | native | 0.167 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.237 |
| Run-Trie | AND | round-robin (LFTJ) | 0.236 |
| Run-Trie | AND | decode + SvS | 1.135 |
| Roaring (roaring crate) | AND | native | 0.054 |
| Roaring (roaring crate) | AND | native, count only | 0.050 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.095 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.099 |
| Roaring (roaring crate) | AND | decode + SvS | 2.434 |
