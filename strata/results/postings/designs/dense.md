
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 2.64 terms, 1787965 postings) |
| mean AND / OR result | 42798 / 1628738 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.07 | 0.737 (SvS + adaptive (SIMD / gallop @64x)) | 4.789 (slices: bitmap) |
| BP128 | 10.07 | 6.38 | 0.0 | 0.03 | 0.88 | 0.75 | 2.083 (decode + SvS) | 6.967 (bitset window (4096)) |
| BP128 (bitpacking crate) | 10.07 | 6.38 | 0.0 | 0.01 | 0.44 | 0.28 | 1.249 (decode + SvS) | 6.162 (bitset window (4096)) |
| OptPFor | 7.13 | 5.09 | 0.0 | 0.14 | 1.11 | 0.85 | 2.267 (decode + SvS) | 7.094 (bitset window (4096)) |
| Elias-Fano | 10.85 | 5.44 | 0.0 | 0.05 | 1.12 | 1.10 | 2.753 (decode + SvS) | 6.823 (bitset window (4096)) |
| PEF | 7.04 | 3.71 | 0.0 | 0.18 | 1.12 | 0.96 | 2.401 (leapfrog (NextGEQ)) | 6.880 (bitset window (4096)) |
| Loom | 6.86 | 3.69 | 0.0 | 0.49 | 0.36 | 0.21 | 0.478 (native) | 1.207 (native) |
| Interpolative | 5.97 | 3.93 | 0.0 | 0.09 | 3.22 | 3.12 | 6.351 (decode + SvS) | 11.191 (bitset window (4096)) |
| Entropic | 4.97 | 3.40 | 14.3 | 1.85 | 0.95 | 0.74 | 2.075 (decode + SvS) | 2.303 (native) |
| Hybrid (bitmap + BP128) | 10.01 | 6.14 | 0.0 | 0.03 | 0.94 | 0.94 | 2.469 (decode + SvS) | 5.009 (native) |
| Roaring | 11.72 | 8.03 | 0.0 | 0.08 | 1.35 | 0.57 | 0.621 (native) | 0.883 (native) |
| Pyramid | 10.35 | 4.62 | 0.0 | 0.35 | 0.71 | 0.28 | 0.772 (native) | 0.879 (native) |
| Roaring (roaring crate) | 11.72 | 8.03 | 0.0 | 0.16 | 5.65 | 5.02 | 1.095 (native) | 7.164 (bitset window (4096)) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (3.40 bits, 2.075 ms) → Loom (3.69 bits, 0.478 ms)
* OR: Entropic (3.40 bits, 2.303 ms) → Loom (3.69 bits, 1.207 ms) → Pyramid (4.62 bits, 0.879 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 2.414 |
| Uncompressed | AND | SvS + merge (branchless) | 3.320 |
| Uncompressed | AND | SvS + galloping | 1.738 |
| Uncompressed | AND | SvS + binary search | 14.156 |
| Uncompressed | AND | SvS + Baeza-Yates | 2.512 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 1.916 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.874 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.737 |
| Uncompressed | AND | Small Adaptive | 3.023 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 2.833 |
| Uncompressed | OR | slices: heap | 13.061 |
| Uncompressed | OR | slices: fold (pairwise merge) | 4.859 |
| Uncompressed | OR | slices: bitmap | 4.789 |
| Uncompressed | AND | leapfrog (NextGEQ) | 2.115 |
| Uncompressed | AND | round-robin (LFTJ) | 2.566 |
| Uncompressed | AND | decode + SvS | 0.851 |
| Uncompressed | OR | heap | 10.793 |
| Uncompressed | OR | min-scan | 7.384 |
| Uncompressed | OR | bitset window (4096) | 4.853 |
| BP128 | AND | leapfrog (NextGEQ) | 4.435 |
| BP128 | AND | round-robin (LFTJ) | 4.986 |
| BP128 | AND | decode + SvS | 2.083 |
| BP128 | OR | heap | 12.983 |
| BP128 | OR | min-scan | 8.451 |
| BP128 | OR | bitset window (4096) | 6.967 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 3.898 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 4.246 |
| BP128 (bitpacking crate) | AND | decode + SvS | 1.249 |
| BP128 (bitpacking crate) | OR | heap | 12.028 |
| BP128 (bitpacking crate) | OR | min-scan | 7.596 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 6.162 |
| OptPFor | AND | leapfrog (NextGEQ) | 4.517 |
| OptPFor | AND | round-robin (LFTJ) | 5.132 |
| OptPFor | AND | decode + SvS | 2.267 |
| OptPFor | OR | heap | 12.988 |
| OptPFor | OR | min-scan | 8.730 |
| OptPFor | OR | bitset window (4096) | 7.094 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 3.691 |
| Elias-Fano | AND | round-robin (LFTJ) | 4.362 |
| Elias-Fano | AND | decode + SvS | 2.753 |
| Elias-Fano | OR | heap | 14.211 |
| Elias-Fano | OR | min-scan | 10.120 |
| Elias-Fano | OR | bitset window (4096) | 6.823 |
| PEF | AND | leapfrog (NextGEQ) | 2.401 |
| PEF | AND | round-robin (LFTJ) | 2.874 |
| PEF | AND | decode + SvS | 2.466 |
| PEF | OR | heap | 14.409 |
| PEF | OR | min-scan | 9.046 |
| PEF | OR | bitset window (4096) | 6.880 |
| Loom | AND | native | 0.478 |
| Loom | OR | native | 1.207 |
| Loom | AND | native, count only | 0.480 |
| Loom | OR | native, count only | 0.766 |
| Loom | AND | leapfrog (NextGEQ) | 2.221 |
| Loom | AND | round-robin (LFTJ) | 2.685 |
| Loom | AND | decode + SvS | 1.102 |
| Loom | OR | heap | 12.968 |
| Loom | OR | min-scan | 8.606 |
| Loom | OR | bitset window (4096) | 6.326 |
| Interpolative | AND | leapfrog (NextGEQ) | 7.475 |
| Interpolative | AND | round-robin (LFTJ) | 9.193 |
| Interpolative | AND | decode + SvS | 6.351 |
| Interpolative | OR | heap | 17.284 |
| Interpolative | OR | min-scan | 12.699 |
| Interpolative | OR | bitset window (4096) | 11.191 |
| Entropic | AND | native | 3.565 |
| Entropic | OR | native | 2.303 |
| Entropic | AND | leapfrog (NextGEQ) | 3.995 |
| Entropic | AND | round-robin (LFTJ) | 5.234 |
| Entropic | AND | decode + SvS | 2.075 |
| Entropic | OR | heap | 17.499 |
| Entropic | OR | min-scan | 16.024 |
| Entropic | OR | bitset window (4096) | 11.732 |
| Hybrid (bitmap + BP128) | AND | native | 2.637 |
| Hybrid (bitmap + BP128) | OR | native | 5.009 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 3.211 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 3.438 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 2.469 |
| Hybrid (bitmap + BP128) | OR | heap | 15.837 |
| Hybrid (bitmap + BP128) | OR | min-scan | 11.544 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 9.284 |
| Roaring | AND | native | 0.621 |
| Roaring | OR | native | 0.883 |
| Roaring | AND | native, count only | 0.618 |
| Roaring | OR | native, count only | 0.434 |
| Roaring | AND | leapfrog (NextGEQ) | 1.698 |
| Roaring | AND | round-robin (LFTJ) | 1.901 |
| Roaring | AND | decode + SvS | 1.646 |
| Roaring | OR | heap | 13.240 |
| Roaring | OR | min-scan | 8.892 |
| Roaring | OR | bitset window (4096) | 6.964 |
| Pyramid | AND | native | 0.772 |
| Pyramid | OR | native | 0.879 |
| Pyramid | AND | native, count only | 0.723 |
| Pyramid | OR | native, count only | 0.357 |
| Pyramid | AND | leapfrog (NextGEQ) | 2.751 |
| Pyramid | AND | round-robin (LFTJ) | 3.254 |
| Pyramid | AND | decode + SvS | 1.244 |
| Pyramid | OR | heap | 17.365 |
| Pyramid | OR | min-scan | 12.839 |
| Pyramid | OR | bitset window (4096) | 12.549 |
| Roaring (roaring crate) | AND | native | 1.095 |
| Roaring (roaring crate) | OR | native | 9.740 |
| Roaring (roaring crate) | AND | native, count only | 0.925 |
| Roaring (roaring crate) | OR | native, count only | 3.065 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 3.773 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 4.100 |
| Roaring (roaring crate) | AND | decode + SvS | 8.366 |
| Roaring (roaring crate) | OR | heap | 16.597 |
| Roaring (roaring crate) | OR | min-scan | 14.284 |
| Roaring (roaring crate) | OR | bitset window (4096) | 7.164 |
