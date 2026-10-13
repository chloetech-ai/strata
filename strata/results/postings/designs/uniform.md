
## Collection and workload

| | |
|---|---|
| collection | synthetic-uniform (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 500 (mean 2.70 terms, 686098 postings) |
| mean AND / OR result | 119 / 675834 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.05 | 0.23 | 0.21 | 0.105 (SvS + adaptive (SIMD / gallop @64x)) | 2.009 (bitset window (4096)) |
| VByte | 13.92 | 10.91 | 0.0 | 0.08 | 2.93 | 2.71 | 0.258 (leapfrog (NextGEQ)) | 3.498 (bitset window (4096)) |
| BP128 | 12.38 | 9.39 | 0.0 | 0.05 | 0.88 | 0.61 | 0.153 (leapfrog (NextGEQ)) | 3.176 (bitset window (4096)) |
| BP128 (bitpacking crate) | 12.38 | 9.39 | 0.0 | 0.03 | 0.77 | 0.42 | 0.137 (leapfrog (NextGEQ)) | 3.056 (bitset window (4096)) |
| OptPFor | 11.89 | 8.87 | 0.0 | 0.15 | 1.05 | 0.80 | 0.176 (leapfrog (NextGEQ)) | 3.024 (bitset window (4096)) |
| Elias-Fano | 10.85 | 8.08 | 0.0 | 0.06 | 1.74 | 1.53 | 0.078 (leapfrog (NextGEQ)) | 3.010 (bitset window (4096)) |
| PEF | 10.90 | 8.08 | 0.0 | 0.14 | 1.61 | 1.73 | 0.080 (leapfrog (NextGEQ)) | 3.626 (bitset window (4096)) |
| Loom | 10.84 | 8.01 | 0.0 | 0.45 | 0.48 | 0.35 | 0.037 (native) | 0.739 (native) |
| Interpolative | 11.33 | 8.52 | 0.0 | 0.15 | 3.37 | 3.47 | 0.377 (leapfrog (NextGEQ)) | 4.856 (bitset window (4096)) |
| Entropic | 10.39 | 7.60 | 1.3 | 1.98 | 1.47 | 1.34 | 0.157 (native) | 1.333 (native) |
| Hybrid (bitmap + BP128) | 12.33 | 9.31 | 0.0 | 0.05 | 1.08 | 0.79 | 0.116 (native) | 2.003 (native) |
| Roaring | 20.42 | 13.70 | 0.0 | 0.20 | 1.93 | 0.38 | 0.034 (native) | 0.480 (native) |
| Pyramid | 18.29 | 10.47 | 0.0 | 0.51 | 1.82 | 0.52 | 0.037 (native) | 0.366 (native) |
| Trie | 20.10 | 13.24 | 0.0 | 1.03 | 16.40 | 9.75 | 0.299 (native) | 3.307 (native) |
| Run-Trie | 20.10 | 13.24 | 0.0 | 1.09 | 20.41 | 11.78 | 0.317 (native) | 3.445 (native) |
| Roaring (roaring crate) | 20.42 | 13.70 | 0.0 | 0.34 | 8.03 | 5.29 | 0.134 (leapfrog (NextGEQ)) | 4.115 (native) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (7.60 bits, 0.157 ms) → Loom (8.01 bits, 0.037 ms) → Roaring (13.70 bits, 0.034 ms)
* OR: Entropic (7.60 bits, 1.333 ms) → Loom (8.01 bits, 0.739 ms) → Pyramid (10.47 bits, 0.366 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.469 |
| Uncompressed | AND | SvS + merge (branchless) | 1.699 |
| Uncompressed | AND | SvS + galloping | 0.118 |
| Uncompressed | AND | SvS + binary search | 0.381 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.179 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.783 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.806 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.609 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.105 |
| Uncompressed | AND | Small Adaptive | 0.131 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.239 |
| Uncompressed | OR | slices: heap | 8.119 |
| Uncompressed | OR | slices: fold (pairwise merge) | 2.732 |
| Uncompressed | OR | slices: bitmap | 2.070 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.118 |
| Uncompressed | AND | round-robin (LFTJ) | 0.210 |
| Uncompressed | AND | decode + SvS | 0.290 |
| Uncompressed | OR | heap | 4.362 |
| Uncompressed | OR | min-scan | 3.483 |
| Uncompressed | OR | bitset window (4096) | 2.009 |
| VByte | AND | leapfrog (NextGEQ) | 0.258 |
| VByte | AND | round-robin (LFTJ) | 0.446 |
| VByte | AND | decode + SvS | 0.937 |
| VByte | OR | heap | 5.848 |
| VByte | OR | min-scan | 4.143 |
| VByte | OR | bitset window (4096) | 3.498 |
| BP128 | AND | leapfrog (NextGEQ) | 0.153 |
| BP128 | AND | round-robin (LFTJ) | 0.243 |
| BP128 | AND | decode + SvS | 0.488 |
| BP128 | OR | heap | 4.964 |
| BP128 | OR | min-scan | 3.811 |
| BP128 | OR | bitset window (4096) | 3.176 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.137 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.216 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.487 |
| BP128 (bitpacking crate) | OR | heap | 5.094 |
| BP128 (bitpacking crate) | OR | min-scan | 3.639 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 3.056 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.176 |
| OptPFor | AND | round-robin (LFTJ) | 0.278 |
| OptPFor | AND | decode + SvS | 0.593 |
| OptPFor | OR | heap | 5.092 |
| OptPFor | OR | min-scan | 3.759 |
| OptPFor | OR | bitset window (4096) | 3.024 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.078 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.129 |
| Elias-Fano | AND | decode + SvS | 1.114 |
| Elias-Fano | OR | heap | 5.298 |
| Elias-Fano | OR | min-scan | 3.325 |
| Elias-Fano | OR | bitset window (4096) | 3.010 |
| PEF | AND | leapfrog (NextGEQ) | 0.080 |
| PEF | AND | round-robin (LFTJ) | 0.136 |
| PEF | AND | decode + SvS | 1.043 |
| PEF | OR | heap | 5.478 |
| PEF | OR | min-scan | 4.295 |
| PEF | OR | bitset window (4096) | 3.626 |
| Loom | AND | native | 0.037 |
| Loom | OR | native | 0.739 |
| Loom | AND | native, count only | 0.038 |
| Loom | OR | native, count only | 0.275 |
| Loom | AND | leapfrog (NextGEQ) | 0.076 |
| Loom | AND | round-robin (LFTJ) | 0.130 |
| Loom | AND | decode + SvS | 0.323 |
| Loom | OR | heap | 5.341 |
| Loom | OR | min-scan | 4.163 |
| Loom | OR | bitset window (4096) | 3.317 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.377 |
| Interpolative | AND | round-robin (LFTJ) | 0.754 |
| Interpolative | AND | decode + SvS | 2.696 |
| Interpolative | OR | heap | 7.274 |
| Interpolative | OR | min-scan | 5.674 |
| Interpolative | OR | bitset window (4096) | 4.856 |
| Entropic | AND | native | 0.157 |
| Entropic | OR | native | 1.333 |
| Entropic | AND | leapfrog (NextGEQ) | 0.174 |
| Entropic | AND | round-robin (LFTJ) | 0.340 |
| Entropic | AND | decode + SvS | 0.926 |
| Entropic | OR | heap | 6.409 |
| Entropic | OR | min-scan | 4.784 |
| Entropic | OR | bitset window (4096) | 4.613 |
| Hybrid (bitmap + BP128) | AND | native | 0.116 |
| Hybrid (bitmap + BP128) | OR | native | 2.003 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.125 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.183 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.926 |
| Hybrid (bitmap + BP128) | OR | heap | 6.323 |
| Hybrid (bitmap + BP128) | OR | min-scan | 5.758 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 5.908 |
| Roaring | AND | native | 0.034 |
| Roaring | OR | native | 0.480 |
| Roaring | AND | native, count only | 0.036 |
| Roaring | OR | native, count only | 0.281 |
| Roaring | AND | leapfrog (NextGEQ) | 0.074 |
| Roaring | AND | round-robin (LFTJ) | 0.113 |
| Roaring | AND | decode + SvS | 0.308 |
| Roaring | OR | heap | 5.689 |
| Roaring | OR | min-scan | 3.933 |
| Roaring | OR | bitset window (4096) | 3.057 |
| Pyramid | AND | native | 0.037 |
| Pyramid | OR | native | 0.366 |
| Pyramid | AND | native, count only | 0.032 |
| Pyramid | OR | native, count only | 0.119 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.085 |
| Pyramid | AND | round-robin (LFTJ) | 0.127 |
| Pyramid | AND | decode + SvS | 0.320 |
| Pyramid | OR | heap | 6.809 |
| Pyramid | OR | min-scan | 5.955 |
| Pyramid | OR | bitset window (4096) | 5.974 |
| Trie | AND | native | 0.299 |
| Trie | OR | native | 3.307 |
| Trie | AND | leapfrog (NextGEQ) | 0.449 |
| Trie | AND | round-robin (LFTJ) | 0.670 |
| Trie | AND | decode + SvS | 1.938 |
| Trie | OR | heap | 8.505 |
| Trie | OR | min-scan | 6.989 |
| Trie | OR | bitset window (4096) | 6.897 |
| Run-Trie | AND | native | 0.317 |
| Run-Trie | OR | native | 3.445 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.466 |
| Run-Trie | AND | round-robin (LFTJ) | 0.697 |
| Run-Trie | AND | decode + SvS | 2.278 |
| Run-Trie | OR | heap | 9.212 |
| Run-Trie | OR | min-scan | 7.539 |
| Run-Trie | OR | bitset window (4096) | 7.153 |
| Roaring (roaring crate) | AND | native | 0.208 |
| Roaring (roaring crate) | OR | native | 4.115 |
| Roaring (roaring crate) | AND | native, count only | 0.222 |
| Roaring (roaring crate) | OR | native, count only | 0.532 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.134 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.173 |
| Roaring (roaring crate) | AND | decode + SvS | 3.806 |
| Roaring (roaring crate) | OR | heap | 7.390 |
| Roaring (roaring crate) | OR | min-scan | 6.187 |
| Roaring (roaring crate) | OR | bitset window (4096) | 5.504 |
