
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 500 (mean 2.80 terms, 576257 postings) |
| mean AND / OR result | 309 / 562995 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.08 | 0.027 (SvS + adaptive (SIMD / gallop @64x)) | 1.320 (slices: fold (pairwise merge)) |
| VByte | 10.33 | 9.61 | 0.0 | 0.03 | 1.52 | 1.35 | 0.092 (leapfrog (NextGEQ)) | 2.246 (min-scan) |
| BP128 | 10.07 | 7.69 | 0.0 | 0.03 | 0.88 | 0.76 | 0.065 (leapfrog (NextGEQ)) | 2.090 (min-scan) |
| BP128 (bitpacking crate) | 10.07 | 7.69 | 0.0 | 0.01 | 0.44 | 0.30 | 0.051 (leapfrog (NextGEQ)) | 1.828 (min-scan) |
| OptPFor | 7.13 | 5.92 | 0.0 | 0.14 | 1.11 | 0.89 | 0.068 (leapfrog (NextGEQ)) | 2.154 (min-scan) |
| Elias-Fano | 10.85 | 8.13 | 0.0 | 0.05 | 1.11 | 1.10 | 0.045 (leapfrog (NextGEQ)) | 2.399 (bitset window (4096)) |
| PEF | 7.04 | 4.94 | 0.0 | 0.18 | 1.12 | 1.01 | 0.035 (leapfrog (NextGEQ)) | 2.208 (min-scan) |
| Loom | 6.86 | 4.90 | 0.0 | 0.49 | 0.36 | 0.24 | 0.013 (native) | 0.513 (native) |
| Interpolative | 5.97 | 4.90 | 0.0 | 0.09 | 3.19 | 3.13 | 0.135 (leapfrog (NextGEQ)) | 3.465 (min-scan) |
| Entropic | 4.97 | 4.22 | 14.3 | 1.85 | 0.95 | 0.78 | 0.064 (native) | 0.897 (native) |
| Hybrid (bitmap + BP128) | 10.01 | 7.58 | 0.0 | 0.03 | 0.94 | 0.86 | 0.052 (native) | 1.326 (native) |
| Roaring | 11.72 | 9.32 | 0.0 | 0.08 | 1.34 | 0.95 | 0.018 (native) | 0.382 (native) |
| Pyramid | 10.35 | 6.20 | 0.0 | 0.34 | 0.71 | 0.45 | 0.016 (native) | 0.356 (native) |
| Trie | 9.61 | 7.18 | 0.0 | 0.39 | 10.73 | 7.94 | 0.103 (native) | 3.660 (native) |
| Run-Trie | 9.13 | 6.67 | 0.0 | 0.43 | 11.60 | 8.67 | 0.109 (native) | 3.863 (native) |
| Roaring (roaring crate) | 11.72 | 9.32 | 0.0 | 0.16 | 5.64 | 5.33 | 0.053 (native) | 2.498 (bitset window (4096)) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.22 bits, 0.064 ms) → Loom (4.90 bits, 0.013 ms)
* OR: Entropic (4.22 bits, 0.897 ms) → Loom (4.90 bits, 0.513 ms) → Pyramid (6.20 bits, 0.356 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.401 |
| Uncompressed | AND | SvS + merge (branchless) | 0.573 |
| Uncompressed | AND | SvS + galloping | 0.029 |
| Uncompressed | AND | SvS + binary search | 0.176 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.048 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.277 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.226 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.027 |
| Uncompressed | AND | Small Adaptive | 0.042 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.052 |
| Uncompressed | OR | slices: heap | 3.665 |
| Uncompressed | OR | slices: fold (pairwise merge) | 1.320 |
| Uncompressed | OR | slices: bitmap | 1.725 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.031 |
| Uncompressed | AND | round-robin (LFTJ) | 0.048 |
| Uncompressed | AND | decode + SvS | 0.062 |
| Uncompressed | OR | heap | 2.146 |
| Uncompressed | OR | min-scan | 1.740 |
| Uncompressed | OR | bitset window (4096) | 1.784 |
| VByte | AND | leapfrog (NextGEQ) | 0.092 |
| VByte | AND | round-robin (LFTJ) | 0.146 |
| VByte | AND | decode + SvS | 0.649 |
| VByte | OR | heap | 2.912 |
| VByte | OR | min-scan | 2.246 |
| VByte | OR | bitset window (4096) | 2.571 |
| BP128 | AND | leapfrog (NextGEQ) | 0.065 |
| BP128 | AND | round-robin (LFTJ) | 0.096 |
| BP128 | AND | decode + SvS | 0.460 |
| BP128 | OR | heap | 2.808 |
| BP128 | OR | min-scan | 2.090 |
| BP128 | OR | bitset window (4096) | 2.438 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.051 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.070 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.190 |
| BP128 (bitpacking crate) | OR | heap | 2.482 |
| BP128 (bitpacking crate) | OR | min-scan | 1.828 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 2.180 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.068 |
| OptPFor | AND | round-robin (LFTJ) | 0.098 |
| OptPFor | AND | decode + SvS | 0.519 |
| OptPFor | OR | heap | 2.822 |
| OptPFor | OR | min-scan | 2.154 |
| OptPFor | OR | bitset window (4096) | 2.489 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.045 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.060 |
| Elias-Fano | AND | decode + SvS | 0.674 |
| Elias-Fano | OR | heap | 3.043 |
| Elias-Fano | OR | min-scan | 2.697 |
| Elias-Fano | OR | bitset window (4096) | 2.399 |
| PEF | AND | leapfrog (NextGEQ) | 0.035 |
| PEF | AND | round-robin (LFTJ) | 0.049 |
| PEF | AND | decode + SvS | 0.582 |
| PEF | OR | heap | 3.180 |
| PEF | OR | min-scan | 2.208 |
| PEF | OR | bitset window (4096) | 2.419 |
| Loom | AND | native | 0.013 |
| Loom | OR | native | 0.513 |
| Loom | AND | native, count only | 0.013 |
| Loom | OR | native, count only | 0.280 |
| Loom | AND | leapfrog (NextGEQ) | 0.033 |
| Loom | AND | round-robin (LFTJ) | 0.046 |
| Loom | AND | decode + SvS | 0.141 |
| Loom | OR | heap | 2.861 |
| Loom | OR | min-scan | 2.291 |
| Loom | OR | bitset window (4096) | 2.245 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.135 |
| Interpolative | AND | round-robin (LFTJ) | 0.232 |
| Interpolative | AND | decode + SvS | 1.836 |
| Interpolative | OR | heap | 4.163 |
| Interpolative | OR | min-scan | 3.465 |
| Interpolative | OR | bitset window (4096) | 3.809 |
| Entropic | AND | native | 0.064 |
| Entropic | OR | native | 0.897 |
| Entropic | AND | leapfrog (NextGEQ) | 0.067 |
| Entropic | AND | round-robin (LFTJ) | 0.115 |
| Entropic | AND | decode + SvS | 0.458 |
| Entropic | OR | heap | 4.209 |
| Entropic | OR | min-scan | 3.827 |
| Entropic | OR | bitset window (4096) | 3.983 |
| Hybrid (bitmap + BP128) | AND | native | 0.052 |
| Hybrid (bitmap + BP128) | OR | native | 1.326 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.055 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.074 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.594 |
| Hybrid (bitmap + BP128) | OR | heap | 4.020 |
| Hybrid (bitmap + BP128) | OR | min-scan | 3.374 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 3.210 |
| Roaring | AND | native | 0.018 |
| Roaring | OR | native | 0.382 |
| Roaring | AND | native, count only | 0.019 |
| Roaring | OR | native, count only | 0.228 |
| Roaring | AND | leapfrog (NextGEQ) | 0.024 |
| Roaring | AND | round-robin (LFTJ) | 0.034 |
| Roaring | AND | decode + SvS | 0.297 |
| Roaring | OR | heap | 3.023 |
| Roaring | OR | min-scan | 2.354 |
| Roaring | OR | bitset window (4096) | 2.439 |
| Pyramid | AND | native | 0.016 |
| Pyramid | OR | native | 0.356 |
| Pyramid | AND | native, count only | 0.015 |
| Pyramid | OR | native, count only | 0.105 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.039 |
| Pyramid | AND | round-robin (LFTJ) | 0.056 |
| Pyramid | AND | decode + SvS | 0.211 |
| Pyramid | OR | heap | 4.508 |
| Pyramid | OR | min-scan | 3.803 |
| Pyramid | OR | bitset window (4096) | 4.257 |
| Trie | AND | native | 0.103 |
| Trie | OR | native | 3.660 |
| Trie | AND | leapfrog (NextGEQ) | 0.178 |
| Trie | AND | round-robin (LFTJ) | 0.238 |
| Trie | AND | decode + SvS | 2.930 |
| Trie | OR | heap | 7.185 |
| Trie | OR | min-scan | 6.208 |
| Trie | OR | bitset window (4096) | 5.864 |
| Run-Trie | AND | native | 0.109 |
| Run-Trie | OR | native | 3.863 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.186 |
| Run-Trie | AND | round-robin (LFTJ) | 0.248 |
| Run-Trie | AND | decode + SvS | 3.146 |
| Run-Trie | OR | heap | 7.476 |
| Run-Trie | OR | min-scan | 6.508 |
| Run-Trie | OR | bitset window (4096) | 6.052 |
| Roaring (roaring crate) | AND | native | 0.053 |
| Roaring (roaring crate) | OR | native | 2.949 |
| Roaring (roaring crate) | AND | native, count only | 0.050 |
| Roaring (roaring crate) | OR | native, count only | 0.605 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.058 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.071 |
| Roaring (roaring crate) | AND | decode + SvS | 2.458 |
| Roaring (roaring crate) | OR | heap | 4.081 |
| Roaring (roaring crate) | OR | min-scan | 4.047 |
| Roaring (roaring crate) | OR | bitset window (4096) | 2.498 |
