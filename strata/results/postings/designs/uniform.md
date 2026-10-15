
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
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.08 | 0.036 (SvS + adaptive (SIMD / gallop @64x)) | 1.638 (slices: fold (pairwise merge)) |
| VByte | 13.92 | 10.91 | 0.0 | 0.05 | 2.23 | 1.82 | 0.201 (leapfrog (NextGEQ)) | 2.791 (min-scan) |
| BP128 | 12.38 | 9.39 | 0.0 | 0.03 | 0.94 | 0.76 | 0.145 (leapfrog (NextGEQ)) | 2.612 (min-scan) |
| BP128 (bitpacking crate) | 12.38 | 9.39 | 0.0 | 0.02 | 0.48 | 0.29 | 0.109 (leapfrog (NextGEQ)) | 2.297 (min-scan) |
| OptPFor | 11.89 | 8.87 | 0.0 | 0.11 | 0.97 | 0.80 | 0.148 (leapfrog (NextGEQ)) | 2.670 (min-scan) |
| Elias-Fano | 10.85 | 8.08 | 0.0 | 0.05 | 1.12 | 1.11 | 0.085 (leapfrog (NextGEQ)) | 2.866 (bitset window (4096)) |
| PEF | 10.90 | 8.08 | 0.0 | 0.18 | 1.15 | 1.12 | 0.077 (leapfrog (NextGEQ)) | 2.856 (min-scan) |
| Loom | 10.84 | 8.01 | 0.0 | 0.49 | 0.36 | 0.33 | 0.026 (native) | 0.758 (native) |
| Interpolative | 11.33 | 8.52 | 0.0 | 0.10 | 2.86 | 2.89 | 0.323 (leapfrog (NextGEQ)) | 4.123 (min-scan) |
| Entropic | 10.39 | 7.60 | 1.3 | 1.26 | 0.96 | 0.77 | 0.142 (native) | 1.062 (native) |
| Hybrid (bitmap + BP128) | 12.33 | 9.31 | 0.0 | 0.04 | 0.99 | 0.84 | 0.105 (native) | 1.470 (native) |
| Roaring | 20.42 | 13.70 | 0.0 | 0.11 | 0.86 | 0.28 | 0.021 (native) | 0.435 (native) |
| Pyramid | 18.29 | 10.47 | 0.0 | 0.65 | 1.17 | 0.64 | 0.030 (native) | 0.497 (native) |
| Trie | 20.10 | 13.24 | 0.0 | 0.72 | 22.81 | 17.09 | 0.255 (native) | 5.489 (native) |
| Run-Trie | 20.10 | 13.24 | 0.0 | 0.74 | 24.68 | 18.36 | 0.265 (native) | 5.751 (native) |
| Roaring (roaring crate) | 20.42 | 13.70 | 0.0 | 0.17 | 5.00 | 4.28 | 0.087 (leapfrog (NextGEQ)) | 2.854 (bitset window (4096)) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (7.60 bits, 0.142 ms) → Loom (8.01 bits, 0.026 ms) → Roaring (13.70 bits, 0.021 ms)
* OR: Entropic (7.60 bits, 1.062 ms) → Loom (8.01 bits, 0.758 ms) → Pyramid (10.47 bits, 0.497 ms) → Roaring (13.70 bits, 0.435 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.509 |
| Uncompressed | AND | SvS + merge (branchless) | 0.755 |
| Uncompressed | AND | SvS + galloping | 0.052 |
| Uncompressed | AND | SvS + binary search | 0.152 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.072 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.434 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.287 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.036 |
| Uncompressed | AND | Small Adaptive | 0.063 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.098 |
| Uncompressed | OR | slices: heap | 4.478 |
| Uncompressed | OR | slices: fold (pairwise merge) | 1.638 |
| Uncompressed | OR | slices: bitmap | 2.077 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.056 |
| Uncompressed | AND | round-robin (LFTJ) | 0.091 |
| Uncompressed | AND | decode + SvS | 0.078 |
| Uncompressed | OR | heap | 2.879 |
| Uncompressed | OR | min-scan | 2.136 |
| Uncompressed | OR | bitset window (4096) | 2.118 |
| VByte | AND | leapfrog (NextGEQ) | 0.201 |
| VByte | AND | round-robin (LFTJ) | 0.346 |
| VByte | AND | decode + SvS | 0.759 |
| VByte | OR | heap | 3.810 |
| VByte | OR | min-scan | 2.791 |
| VByte | OR | bitset window (4096) | 3.075 |
| BP128 | AND | leapfrog (NextGEQ) | 0.145 |
| BP128 | AND | round-robin (LFTJ) | 0.248 |
| BP128 | AND | decode + SvS | 0.547 |
| BP128 | OR | heap | 3.651 |
| BP128 | OR | min-scan | 2.612 |
| BP128 | OR | bitset window (4096) | 2.920 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.109 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.167 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.226 |
| BP128 (bitpacking crate) | OR | heap | 3.332 |
| BP128 (bitpacking crate) | OR | min-scan | 2.297 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 2.610 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.148 |
| OptPFor | AND | round-robin (LFTJ) | 0.255 |
| OptPFor | AND | decode + SvS | 0.572 |
| OptPFor | OR | heap | 3.669 |
| OptPFor | OR | min-scan | 2.670 |
| OptPFor | OR | bitset window (4096) | 2.948 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.085 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.127 |
| Elias-Fano | AND | decode + SvS | 0.809 |
| Elias-Fano | OR | heap | 3.968 |
| Elias-Fano | OR | min-scan | 3.360 |
| Elias-Fano | OR | bitset window (4096) | 2.866 |
| PEF | AND | leapfrog (NextGEQ) | 0.077 |
| PEF | AND | round-robin (LFTJ) | 0.113 |
| PEF | AND | decode + SvS | 0.756 |
| PEF | OR | heap | 4.265 |
| PEF | OR | min-scan | 2.856 |
| PEF | OR | bitset window (4096) | 2.952 |
| Loom | AND | native | 0.026 |
| Loom | OR | native | 0.758 |
| Loom | AND | native, count only | 0.026 |
| Loom | OR | native, count only | 0.459 |
| Loom | AND | leapfrog (NextGEQ) | 0.074 |
| Loom | AND | round-robin (LFTJ) | 0.107 |
| Loom | AND | decode + SvS | 0.222 |
| Loom | OR | heap | 3.883 |
| Loom | OR | min-scan | 3.023 |
| Loom | OR | bitset window (4096) | 2.726 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.323 |
| Interpolative | AND | round-robin (LFTJ) | 0.643 |
| Interpolative | AND | decode + SvS | 2.066 |
| Interpolative | OR | heap | 5.241 |
| Interpolative | OR | min-scan | 4.123 |
| Interpolative | OR | bitset window (4096) | 4.422 |
| Entropic | AND | native | 0.142 |
| Entropic | OR | native | 1.062 |
| Entropic | AND | leapfrog (NextGEQ) | 0.153 |
| Entropic | AND | round-robin (LFTJ) | 0.309 |
| Entropic | AND | decode + SvS | 0.547 |
| Entropic | OR | heap | 5.426 |
| Entropic | OR | min-scan | 4.624 |
| Entropic | OR | bitset window (4096) | 4.745 |
| Hybrid (bitmap + BP128) | AND | native | 0.105 |
| Hybrid (bitmap + BP128) | OR | native | 1.470 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.115 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.168 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.704 |
| Hybrid (bitmap + BP128) | OR | heap | 5.294 |
| Hybrid (bitmap + BP128) | OR | min-scan | 4.263 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 3.942 |
| Roaring | AND | native | 0.021 |
| Roaring | OR | native | 0.435 |
| Roaring | AND | native, count only | 0.023 |
| Roaring | OR | native, count only | 0.232 |
| Roaring | AND | leapfrog (NextGEQ) | 0.045 |
| Roaring | AND | round-robin (LFTJ) | 0.064 |
| Roaring | AND | decode + SvS | 0.252 |
| Roaring | OR | heap | 3.864 |
| Roaring | OR | min-scan | 2.835 |
| Roaring | OR | bitset window (4096) | 2.925 |
| Pyramid | AND | native | 0.030 |
| Pyramid | OR | native | 0.497 |
| Pyramid | AND | native, count only | 0.030 |
| Pyramid | OR | native, count only | 0.157 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.092 |
| Pyramid | AND | round-robin (LFTJ) | 0.138 |
| Pyramid | AND | decode + SvS | 0.314 |
| Pyramid | OR | heap | 5.868 |
| Pyramid | OR | min-scan | 4.828 |
| Pyramid | OR | bitset window (4096) | 5.245 |
| Trie | AND | native | 0.255 |
| Trie | OR | native | 5.489 |
| Trie | AND | leapfrog (NextGEQ) | 0.446 |
| Trie | AND | round-robin (LFTJ) | 0.631 |
| Trie | AND | decode + SvS | 4.532 |
| Trie | OR | heap | 10.716 |
| Trie | OR | min-scan | 9.248 |
| Trie | OR | bitset window (4096) | 8.723 |
| Run-Trie | AND | native | 0.265 |
| Run-Trie | OR | native | 5.751 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.467 |
| Run-Trie | AND | round-robin (LFTJ) | 0.661 |
| Run-Trie | AND | decode + SvS | 4.803 |
| Run-Trie | OR | heap | 11.058 |
| Run-Trie | OR | min-scan | 9.648 |
| Run-Trie | OR | bitset window (4096) | 8.954 |
| Roaring (roaring crate) | AND | native | 0.129 |
| Roaring (roaring crate) | OR | native | 3.158 |
| Roaring (roaring crate) | AND | native, count only | 0.129 |
| Roaring (roaring crate) | OR | native, count only | 0.384 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.087 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.113 |
| Roaring (roaring crate) | AND | decode + SvS | 2.839 |
| Roaring (roaring crate) | OR | heap | 4.999 |
| Roaring (roaring crate) | OR | min-scan | 4.597 |
| Roaring (roaring crate) | OR | bitset window (4096) | 2.854 |
