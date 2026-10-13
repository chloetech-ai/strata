
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
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.10 | 0.10 | 0.030 (SvS + adaptive (SIMD / gallop @64x)) | 1.326 (slices: fold (pairwise merge)) |
| VByte | 10.33 | 9.61 | 0.0 | 0.03 | 1.53 | 1.35 | 0.092 (leapfrog (NextGEQ)) | 2.249 (min-scan) |
| BP128 | 10.07 | 7.69 | 0.0 | 0.03 | 0.89 | 0.77 | 0.065 (leapfrog (NextGEQ)) | 2.092 (min-scan) |
| BP128 (bitpacking crate) | 10.07 | 7.69 | 0.0 | 0.01 | 0.44 | 0.30 | 0.051 (leapfrog (NextGEQ)) | 1.825 (min-scan) |
| OptPFor | 7.13 | 5.92 | 0.0 | 0.14 | 1.13 | 0.89 | 0.068 (leapfrog (NextGEQ)) | 2.148 (min-scan) |
| Elias-Fano | 10.85 | 8.13 | 0.0 | 0.05 | 1.11 | 1.10 | 0.044 (leapfrog (NextGEQ)) | 2.401 (bitset window (4096)) |
| PEF | 7.04 | 4.94 | 0.0 | 0.18 | 1.11 | 1.00 | 0.035 (leapfrog (NextGEQ)) | 2.207 (min-scan) |
| Loom | 6.86 | 4.90 | 0.0 | 0.49 | 0.36 | 0.24 | 0.013 (native) | 0.514 (native) |
| Interpolative | 5.97 | 4.90 | 0.0 | 0.09 | 3.22 | 3.13 | 0.135 (leapfrog (NextGEQ)) | 3.467 (min-scan) |
| Entropic | 4.97 | 4.22 | 14.3 | 1.84 | 0.97 | 0.83 | 0.066 (native) | 0.895 (native) |
| Hybrid (bitmap + BP128) | 10.01 | 7.58 | 0.0 | 0.03 | 0.94 | 0.85 | 0.052 (native) | 1.329 (native) |
| Roaring | 11.72 | 9.32 | 0.0 | 0.08 | 1.34 | 0.95 | 0.018 (native) | 0.384 (native) |
| Pyramid | 10.35 | 6.20 | 0.0 | 0.34 | 0.71 | 0.45 | 0.016 (native) | 0.355 (native) |
| Trie | 9.61 | 7.18 | 0.0 | 0.39 | 10.65 | 7.93 | 0.104 (native) | 3.662 (native) |
| Run-Trie | 9.13 | 6.67 | 0.0 | 0.43 | 11.48 | 8.65 | 0.108 (native) | 3.871 (native) |
| Roaring (roaring crate) | 11.72 | 9.32 | 0.0 | 0.16 | 5.63 | 5.19 | 0.053 (native) | 2.497 (bitset window (4096)) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.22 bits, 0.066 ms) → Loom (4.90 bits, 0.013 ms)
* OR: Entropic (4.22 bits, 0.895 ms) → Loom (4.90 bits, 0.514 ms) → Pyramid (6.20 bits, 0.355 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.418 |
| Uncompressed | AND | SvS + merge (branchless) | 0.607 |
| Uncompressed | AND | SvS + galloping | 0.034 |
| Uncompressed | AND | SvS + binary search | 0.203 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.060 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.291 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.230 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.030 |
| Uncompressed | AND | Small Adaptive | 0.047 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.059 |
| Uncompressed | OR | slices: heap | 4.675 |
| Uncompressed | OR | slices: fold (pairwise merge) | 1.326 |
| Uncompressed | OR | slices: bitmap | 1.766 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.032 |
| Uncompressed | AND | round-robin (LFTJ) | 0.050 |
| Uncompressed | AND | decode + SvS | 0.066 |
| Uncompressed | OR | heap | 2.146 |
| Uncompressed | OR | min-scan | 1.744 |
| Uncompressed | OR | bitset window (4096) | 1.788 |
| VByte | AND | leapfrog (NextGEQ) | 0.092 |
| VByte | AND | round-robin (LFTJ) | 0.148 |
| VByte | AND | decode + SvS | 0.647 |
| VByte | OR | heap | 2.912 |
| VByte | OR | min-scan | 2.249 |
| VByte | OR | bitset window (4096) | 2.582 |
| BP128 | AND | leapfrog (NextGEQ) | 0.065 |
| BP128 | AND | round-robin (LFTJ) | 0.099 |
| BP128 | AND | decode + SvS | 0.460 |
| BP128 | OR | heap | 2.829 |
| BP128 | OR | min-scan | 2.092 |
| BP128 | OR | bitset window (4096) | 2.446 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.051 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.070 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.190 |
| BP128 (bitpacking crate) | OR | heap | 2.479 |
| BP128 (bitpacking crate) | OR | min-scan | 1.825 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 2.187 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.068 |
| OptPFor | AND | round-robin (LFTJ) | 0.101 |
| OptPFor | AND | decode + SvS | 0.520 |
| OptPFor | OR | heap | 2.814 |
| OptPFor | OR | min-scan | 2.148 |
| OptPFor | OR | bitset window (4096) | 2.482 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.044 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.060 |
| Elias-Fano | AND | decode + SvS | 0.671 |
| Elias-Fano | OR | heap | 3.042 |
| Elias-Fano | OR | min-scan | 2.696 |
| Elias-Fano | OR | bitset window (4096) | 2.401 |
| PEF | AND | leapfrog (NextGEQ) | 0.035 |
| PEF | AND | round-robin (LFTJ) | 0.049 |
| PEF | AND | decode + SvS | 0.582 |
| PEF | OR | heap | 3.179 |
| PEF | OR | min-scan | 2.207 |
| PEF | OR | bitset window (4096) | 2.420 |
| Loom | AND | native | 0.013 |
| Loom | OR | native | 0.514 |
| Loom | AND | native, count only | 0.013 |
| Loom | OR | native, count only | 0.280 |
| Loom | AND | leapfrog (NextGEQ) | 0.033 |
| Loom | AND | round-robin (LFTJ) | 0.046 |
| Loom | AND | decode + SvS | 0.141 |
| Loom | OR | heap | 2.857 |
| Loom | OR | min-scan | 2.289 |
| Loom | OR | bitset window (4096) | 2.245 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.135 |
| Interpolative | AND | round-robin (LFTJ) | 0.231 |
| Interpolative | AND | decode + SvS | 1.836 |
| Interpolative | OR | heap | 4.148 |
| Interpolative | OR | min-scan | 3.467 |
| Interpolative | OR | bitset window (4096) | 3.810 |
| Entropic | AND | native | 0.066 |
| Entropic | OR | native | 0.895 |
| Entropic | AND | leapfrog (NextGEQ) | 0.067 |
| Entropic | AND | round-robin (LFTJ) | 0.115 |
| Entropic | AND | decode + SvS | 0.458 |
| Entropic | OR | heap | 4.222 |
| Entropic | OR | min-scan | 3.827 |
| Entropic | OR | bitset window (4096) | 3.980 |
| Hybrid (bitmap + BP128) | AND | native | 0.052 |
| Hybrid (bitmap + BP128) | OR | native | 1.329 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.055 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.074 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.593 |
| Hybrid (bitmap + BP128) | OR | heap | 4.023 |
| Hybrid (bitmap + BP128) | OR | min-scan | 3.370 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 3.227 |
| Roaring | AND | native | 0.018 |
| Roaring | OR | native | 0.384 |
| Roaring | AND | native, count only | 0.018 |
| Roaring | OR | native, count only | 0.231 |
| Roaring | AND | leapfrog (NextGEQ) | 0.024 |
| Roaring | AND | round-robin (LFTJ) | 0.034 |
| Roaring | AND | decode + SvS | 0.296 |
| Roaring | OR | heap | 3.020 |
| Roaring | OR | min-scan | 2.359 |
| Roaring | OR | bitset window (4096) | 2.440 |
| Pyramid | AND | native | 0.016 |
| Pyramid | OR | native | 0.355 |
| Pyramid | AND | native, count only | 0.015 |
| Pyramid | OR | native, count only | 0.105 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.039 |
| Pyramid | AND | round-robin (LFTJ) | 0.056 |
| Pyramid | AND | decode + SvS | 0.210 |
| Pyramid | OR | heap | 4.500 |
| Pyramid | OR | min-scan | 3.800 |
| Pyramid | OR | bitset window (4096) | 4.369 |
| Trie | AND | native | 0.104 |
| Trie | OR | native | 3.662 |
| Trie | AND | leapfrog (NextGEQ) | 0.179 |
| Trie | AND | round-robin (LFTJ) | 0.239 |
| Trie | AND | decode + SvS | 2.938 |
| Trie | OR | heap | 7.238 |
| Trie | OR | min-scan | 6.256 |
| Trie | OR | bitset window (4096) | 5.902 |
| Run-Trie | AND | native | 0.108 |
| Run-Trie | OR | native | 3.871 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.186 |
| Run-Trie | AND | round-robin (LFTJ) | 0.248 |
| Run-Trie | AND | decode + SvS | 3.152 |
| Run-Trie | OR | heap | 7.473 |
| Run-Trie | OR | min-scan | 6.518 |
| Run-Trie | OR | bitset window (4096) | 6.060 |
| Roaring (roaring crate) | AND | native | 0.053 |
| Roaring (roaring crate) | OR | native | 2.947 |
| Roaring (roaring crate) | AND | native, count only | 0.051 |
| Roaring (roaring crate) | OR | native, count only | 0.604 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.057 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.071 |
| Roaring (roaring crate) | AND | decode + SvS | 2.454 |
| Roaring (roaring crate) | OR | heap | 4.081 |
| Roaring (roaring crate) | OR | min-scan | 4.037 |
| Roaring (roaring crate) | OR | bitset window (4096) | 2.497 |
