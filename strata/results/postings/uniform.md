
## Collection and workload

| | |
|---|---|
| collection | synthetic-uniform (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 500 (mean 2.70 terms, 686098 postings) |
| mean AND / OR result | 119 / 675834 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.08 | 0.035 (SvS + adaptive (SIMD / gallop @64x)) | 1.601 (slices: fold (pairwise merge)) |
| VByte | 13.92 | 10.91 | 0.0 | 0.05 | 2.22 | 1.78 | 0.195 (leapfrog (NextGEQ)) | 2.787 (min-scan) |
| BP128 | 12.38 | 9.39 | 0.0 | 0.03 | 0.94 | 0.76 | 0.145 (leapfrog (NextGEQ)) | 2.610 (min-scan) |
| BP128 (bitpacking crate) | 12.38 | 9.39 | 0.0 | 0.02 | 0.48 | 0.29 | 0.106 (leapfrog (NextGEQ)) | 2.299 (min-scan) |
| OptPFor | 11.89 | 8.87 | 0.0 | 0.10 | 0.97 | 0.80 | 0.144 (leapfrog (NextGEQ)) | 2.671 (min-scan) |
| Elias-Fano | 10.85 | 8.08 | 0.0 | 0.05 | 1.13 | 1.11 | 0.084 (leapfrog (NextGEQ)) | 2.864 (bitset window (4096)) |
| PEF | 10.90 | 8.08 | 0.0 | 0.18 | 1.14 | 1.12 | 0.076 (leapfrog (NextGEQ)) | 2.862 (min-scan) |
| Loom | 10.84 | 8.01 | 0.0 | 0.49 | 0.36 | 0.33 | 0.025 (native) | 0.755 (native) |
| Interpolative | 11.33 | 8.52 | 0.0 | 0.10 | 2.85 | 2.88 | 0.322 (leapfrog (NextGEQ)) | 4.114 (min-scan) |
| Entropic | 10.39 | 7.60 | 1.3 | 1.27 | 0.96 | 0.76 | 0.141 (native) | 1.063 (native) |
| Hybrid (bitmap + BP128) | 12.33 | 9.31 | 0.0 | 0.04 | 0.99 | 0.84 | 0.105 (native) | 1.469 (native) |
| Roaring | 20.42 | 13.70 | 0.0 | 0.11 | 0.86 | 0.28 | 0.021 (native) | 0.443 (native) |
| Pyramid | 18.29 | 10.47 | 0.0 | 0.64 | 1.16 | 0.63 | 0.030 (native) | 0.496 (native) |
| Trie | 20.10 | 13.24 | 0.0 | 0.72 | 22.69 | 16.97 | 0.256 (native) | 5.481 (native) |
| Run-Trie | 20.10 | 13.24 | 0.0 | 0.74 | 24.63 | 18.38 | 0.265 (native) | 5.742 (native) |
| Roaring (roaring crate) | 20.42 | 13.70 | 0.0 | 0.17 | 4.97 | 4.33 | 0.086 (leapfrog (NextGEQ)) | 2.854 (bitset window (4096)) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (7.60 bits, 0.141 ms) → Loom (8.01 bits, 0.025 ms) → Roaring (13.70 bits, 0.021 ms)
* OR: Entropic (7.60 bits, 1.063 ms) → Loom (8.01 bits, 0.755 ms) → Pyramid (10.47 bits, 0.496 ms) → Roaring (13.70 bits, 0.443 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.497 |
| Uncompressed | AND | SvS + merge (branchless) | 0.753 |
| Uncompressed | AND | SvS + galloping | 0.050 |
| Uncompressed | AND | SvS + binary search | 0.150 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.072 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.427 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.288 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.035 |
| Uncompressed | AND | Small Adaptive | 0.060 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.095 |
| Uncompressed | OR | slices: heap | 4.475 |
| Uncompressed | OR | slices: fold (pairwise merge) | 1.601 |
| Uncompressed | OR | slices: bitmap | 2.040 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.054 |
| Uncompressed | AND | round-robin (LFTJ) | 0.088 |
| Uncompressed | AND | decode + SvS | 0.077 |
| Uncompressed | OR | heap | 2.876 |
| Uncompressed | OR | min-scan | 2.135 |
| Uncompressed | OR | bitset window (4096) | 2.090 |
| VByte | AND | leapfrog (NextGEQ) | 0.195 |
| VByte | AND | round-robin (LFTJ) | 0.342 |
| VByte | AND | decode + SvS | 0.751 |
| VByte | OR | heap | 3.812 |
| VByte | OR | min-scan | 2.787 |
| VByte | OR | bitset window (4096) | 3.070 |
| BP128 | AND | leapfrog (NextGEQ) | 0.145 |
| BP128 | AND | round-robin (LFTJ) | 0.248 |
| BP128 | AND | decode + SvS | 0.547 |
| BP128 | OR | heap | 3.650 |
| BP128 | OR | min-scan | 2.610 |
| BP128 | OR | bitset window (4096) | 2.916 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.106 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.162 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.227 |
| BP128 (bitpacking crate) | OR | heap | 3.322 |
| BP128 (bitpacking crate) | OR | min-scan | 2.299 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 2.601 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.144 |
| OptPFor | AND | round-robin (LFTJ) | 0.249 |
| OptPFor | AND | decode + SvS | 0.571 |
| OptPFor | OR | heap | 3.674 |
| OptPFor | OR | min-scan | 2.671 |
| OptPFor | OR | bitset window (4096) | 2.938 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.084 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.127 |
| Elias-Fano | AND | decode + SvS | 0.808 |
| Elias-Fano | OR | heap | 3.970 |
| Elias-Fano | OR | min-scan | 3.354 |
| Elias-Fano | OR | bitset window (4096) | 2.864 |
| PEF | AND | leapfrog (NextGEQ) | 0.076 |
| PEF | AND | round-robin (LFTJ) | 0.113 |
| PEF | AND | decode + SvS | 0.753 |
| PEF | OR | heap | 4.249 |
| PEF | OR | min-scan | 2.862 |
| PEF | OR | bitset window (4096) | 2.949 |
| Loom | AND | native | 0.025 |
| Loom | OR | native | 0.755 |
| Loom | AND | native, count only | 0.026 |
| Loom | OR | native, count only | 0.457 |
| Loom | AND | leapfrog (NextGEQ) | 0.074 |
| Loom | AND | round-robin (LFTJ) | 0.106 |
| Loom | AND | decode + SvS | 0.221 |
| Loom | OR | heap | 3.874 |
| Loom | OR | min-scan | 3.023 |
| Loom | OR | bitset window (4096) | 2.722 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.322 |
| Interpolative | AND | round-robin (LFTJ) | 0.643 |
| Interpolative | AND | decode + SvS | 2.067 |
| Interpolative | OR | heap | 5.236 |
| Interpolative | OR | min-scan | 4.114 |
| Interpolative | OR | bitset window (4096) | 4.421 |
| Entropic | AND | native | 0.141 |
| Entropic | OR | native | 1.063 |
| Entropic | AND | leapfrog (NextGEQ) | 0.152 |
| Entropic | AND | round-robin (LFTJ) | 0.302 |
| Entropic | AND | decode + SvS | 0.546 |
| Entropic | OR | heap | 5.422 |
| Entropic | OR | min-scan | 4.604 |
| Entropic | OR | bitset window (4096) | 4.744 |
| Hybrid (bitmap + BP128) | AND | native | 0.105 |
| Hybrid (bitmap + BP128) | OR | native | 1.469 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.109 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.169 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.700 |
| Hybrid (bitmap + BP128) | OR | heap | 5.296 |
| Hybrid (bitmap + BP128) | OR | min-scan | 4.249 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 3.924 |
| Roaring | AND | native | 0.021 |
| Roaring | OR | native | 0.443 |
| Roaring | AND | native, count only | 0.023 |
| Roaring | OR | native, count only | 0.228 |
| Roaring | AND | leapfrog (NextGEQ) | 0.045 |
| Roaring | AND | round-robin (LFTJ) | 0.064 |
| Roaring | AND | decode + SvS | 0.253 |
| Roaring | OR | heap | 3.862 |
| Roaring | OR | min-scan | 2.831 |
| Roaring | OR | bitset window (4096) | 2.923 |
| Pyramid | AND | native | 0.030 |
| Pyramid | OR | native | 0.496 |
| Pyramid | AND | native, count only | 0.031 |
| Pyramid | OR | native, count only | 0.157 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.092 |
| Pyramid | AND | round-robin (LFTJ) | 0.138 |
| Pyramid | AND | decode + SvS | 0.314 |
| Pyramid | OR | heap | 5.868 |
| Pyramid | OR | min-scan | 4.816 |
| Pyramid | OR | bitset window (4096) | 5.220 |
| Trie | AND | native | 0.256 |
| Trie | OR | native | 5.481 |
| Trie | AND | leapfrog (NextGEQ) | 0.443 |
| Trie | AND | round-robin (LFTJ) | 0.627 |
| Trie | AND | decode + SvS | 4.523 |
| Trie | OR | heap | 10.711 |
| Trie | OR | min-scan | 9.215 |
| Trie | OR | bitset window (4096) | 8.703 |
| Run-Trie | AND | native | 0.265 |
| Run-Trie | OR | native | 5.742 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.467 |
| Run-Trie | AND | round-robin (LFTJ) | 0.658 |
| Run-Trie | AND | decode + SvS | 4.793 |
| Run-Trie | OR | heap | 11.052 |
| Run-Trie | OR | min-scan | 9.617 |
| Run-Trie | OR | bitset window (4096) | 8.940 |
| Roaring (roaring crate) | AND | native | 0.126 |
| Roaring (roaring crate) | OR | native | 3.158 |
| Roaring (roaring crate) | AND | native, count only | 0.127 |
| Roaring (roaring crate) | OR | native, count only | 0.376 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.086 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.113 |
| Roaring (roaring crate) | AND | decode + SvS | 2.828 |
| Roaring (roaring crate) | OR | heap | 4.996 |
| Roaring (roaring crate) | OR | min-scan | 4.659 |
| Roaring (roaring crate) | OR | bitset window (4096) | 2.854 |
