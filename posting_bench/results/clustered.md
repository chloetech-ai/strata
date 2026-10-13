
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 500 (mean 2.80 terms, 576257 postings) |
| mean AND / OR result | 309 / 562995 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.05 | 0.22 | 0.23 | 0.061 (SvS + galloping) | 1.861 (slices: bitmap) |
| VByte | 10.33 | 9.61 | 0.06 | 2.00 | 1.93 | 0.133 (leapfrog (NextGEQ)) | 3.204 (bitset window (4096)) |
| BP128 | 10.07 | 7.69 | 0.05 | 0.89 | 0.66 | 0.080 (leapfrog (NextGEQ)) | 2.889 (bitset window (4096)) |
| BP128 (bitpacking crate) | 10.07 | 7.69 | 0.03 | 0.67 | 0.42 | 0.072 (leapfrog (NextGEQ)) | 2.539 (min-scan) |
| OptPFor | 7.13 | 5.92 | 0.23 | 1.35 | 1.03 | 0.088 (leapfrog (NextGEQ)) | 2.998 (min-scan) |
| Elias-Fano | 10.85 | 8.13 | 0.07 | 1.92 | 1.84 | 0.055 (leapfrog (NextGEQ)) | 2.873 (bitset window (4096)) |
| PEF | 7.04 | 4.94 | 0.16 | 1.70 | 1.49 | 0.046 (leapfrog (NextGEQ)) | 2.841 (bitset window (4096)) |
| Interpolative | 5.97 | 4.90 | 0.15 | 4.46 | 4.25 | 0.181 (leapfrog (NextGEQ)) | 5.002 (min-scan) |
| Hybrid (bitmap + BP128) | 10.01 | 7.58 | 0.06 | 1.05 | 0.92 | 0.067 (native) | 1.905 (native) |
| Roaring | 11.72 | 9.32 | 0.14 | 2.54 | 1.84 | 0.030 (native) | 0.570 (native) |
| Trie | 9.61 | 7.18 | 0.65 | 8.15 | 5.28 | 0.147 (native) | 2.497 (native) |
| Run-Trie | 9.13 | 6.67 | 0.75 | 8.97 | 6.17 | 0.132 (native) | 2.838 (native) |
| Roaring (roaring crate) | 11.72 | 9.32 | 0.24 | 7.98 | 6.85 | 0.088 (native) | 4.300 (native) |

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.461 |
| Uncompressed | AND | SvS + merge (branchless) | 1.519 |
| Uncompressed | AND | SvS + galloping | 0.061 |
| Uncompressed | AND | SvS + binary search | 0.330 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.111 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.631 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.625 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.479 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.064 |
| Uncompressed | AND | Small Adaptive | 0.072 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.105 |
| Uncompressed | OR | slices: heap | 7.027 |
| Uncompressed | OR | slices: fold (pairwise merge) | 2.378 |
| Uncompressed | OR | slices: bitmap | 1.861 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.068 |
| Uncompressed | AND | round-robin (LFTJ) | 0.109 |
| Uncompressed | AND | decode + SvS | 0.227 |
| Uncompressed | OR | heap | 3.492 |
| Uncompressed | OR | min-scan | 2.979 |
| Uncompressed | OR | bitset window (4096) | 2.027 |
| VByte | AND | leapfrog (NextGEQ) | 0.133 |
| VByte | AND | round-robin (LFTJ) | 0.216 |
| VByte | AND | decode + SvS | 0.785 |
| VByte | OR | heap | 4.526 |
| VByte | OR | min-scan | 3.854 |
| VByte | OR | bitset window (4096) | 3.204 |
| BP128 | AND | leapfrog (NextGEQ) | 0.080 |
| BP128 | AND | round-robin (LFTJ) | 0.117 |
| BP128 | AND | decode + SvS | 0.474 |
| BP128 | OR | heap | 4.250 |
| BP128 | OR | min-scan | 2.973 |
| BP128 | OR | bitset window (4096) | 2.889 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.072 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.101 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.304 |
| BP128 (bitpacking crate) | OR | heap | 4.095 |
| BP128 (bitpacking crate) | OR | min-scan | 2.539 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 2.638 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.088 |
| OptPFor | AND | round-robin (LFTJ) | 0.128 |
| OptPFor | AND | decode + SvS | 0.552 |
| OptPFor | OR | heap | 4.371 |
| OptPFor | OR | min-scan | 2.998 |
| OptPFor | OR | bitset window (4096) | 3.032 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.055 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.079 |
| Elias-Fano | AND | decode + SvS | 0.983 |
| Elias-Fano | OR | heap | 4.428 |
| Elias-Fano | OR | min-scan | 3.172 |
| Elias-Fano | OR | bitset window (4096) | 2.873 |
| PEF | AND | leapfrog (NextGEQ) | 0.046 |
| PEF | AND | round-robin (LFTJ) | 0.069 |
| PEF | AND | decode + SvS | 0.848 |
| PEF | OR | heap | 4.687 |
| PEF | OR | min-scan | 3.300 |
| PEF | OR | bitset window (4096) | 2.841 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.181 |
| Interpolative | AND | round-robin (LFTJ) | 0.314 |
| Interpolative | AND | decode + SvS | 2.803 |
| Interpolative | OR | heap | 6.718 |
| Interpolative | OR | min-scan | 5.002 |
| Interpolative | OR | bitset window (4096) | 5.071 |
| Hybrid (bitmap + BP128) | AND | native | 0.067 |
| Hybrid (bitmap + BP128) | OR | native | 1.905 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.077 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.090 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.767 |
| Hybrid (bitmap + BP128) | OR | heap | 5.578 |
| Hybrid (bitmap + BP128) | OR | min-scan | 4.773 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 5.151 |
| Roaring | AND | native | 0.030 |
| Roaring | OR | native | 0.570 |
| Roaring | AND | native, count only | 0.029 |
| Roaring | OR | native, count only | 0.383 |
| Roaring | AND | leapfrog (NextGEQ) | 0.042 |
| Roaring | AND | round-robin (LFTJ) | 0.073 |
| Roaring | AND | decode + SvS | 0.394 |
| Roaring | OR | heap | 4.607 |
| Roaring | OR | min-scan | 3.761 |
| Roaring | OR | bitset window (4096) | 2.959 |
| Trie | AND | native | 0.147 |
| Trie | OR | native | 2.497 |
| Trie | AND | leapfrog (NextGEQ) | 0.196 |
| Trie | AND | round-robin (LFTJ) | 0.267 |
| Trie | AND | decode + SvS | 1.403 |
| Trie | OR | heap | 7.089 |
| Trie | OR | min-scan | 5.401 |
| Trie | OR | bitset window (4096) | 5.525 |
| Run-Trie | AND | native | 0.132 |
| Run-Trie | OR | native | 2.838 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.202 |
| Run-Trie | AND | round-robin (LFTJ) | 0.279 |
| Run-Trie | AND | decode + SvS | 1.677 |
| Run-Trie | OR | heap | 7.015 |
| Run-Trie | OR | min-scan | 6.073 |
| Run-Trie | OR | bitset window (4096) | 5.858 |
| Roaring (roaring crate) | AND | native | 0.088 |
| Roaring (roaring crate) | OR | native | 4.300 |
| Roaring (roaring crate) | AND | native, count only | 0.086 |
| Roaring (roaring crate) | OR | native, count only | 0.746 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.092 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.125 |
| Roaring (roaring crate) | AND | decode + SvS | 3.603 |
| Roaring (roaring crate) | OR | heap | 5.962 |
| Roaring (roaring crate) | OR | min-scan | 5.488 |
| Roaring (roaring crate) | OR | bitset window (4096) | 4.751 |
