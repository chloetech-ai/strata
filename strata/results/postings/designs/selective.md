
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 500 (mean 2.79 terms, 13800 postings) |
| mean AND / OR result | 81 / 13274 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.11 | 0.004 (SvS + adaptive (SIMD / gallop @64x)) | 0.038 (slices: fold (pairwise merge)) |
| BP128 | 10.07 | 9.94 | 0.0 | 0.03 | 0.91 | 0.80 | 0.015 (decode + SvS) | 0.064 (min-scan) |
| BP128 (bitpacking crate) | 10.07 | 9.94 | 0.0 | 0.01 | 0.44 | 0.33 | 0.009 (decode + SvS) | 0.057 (min-scan) |
| OptPFor | 7.13 | 7.25 | 0.0 | 0.14 | 1.13 | 0.99 | 0.018 (decode + SvS) | 0.067 (min-scan) |
| Elias-Fano | 10.85 | 12.54 | 0.0 | 0.05 | 1.11 | 1.12 | 0.015 (leapfrog (NextGEQ)) | 0.076 (min-scan) |
| PEF | 7.04 | 7.03 | 0.0 | 0.18 | 1.11 | 1.13 | 0.013 (leapfrog (NextGEQ)) | 0.068 (min-scan) |
| Loom | 6.86 | 6.99 | 0.0 | 0.49 | 0.36 | 0.31 | 0.005 (native) | 0.070 (min-scan) |
| Interpolative | 5.97 | 6.30 | 0.0 | 0.09 | 3.22 | 3.20 | 0.030 (leapfrog (NextGEQ)) | 0.098 (min-scan) |
| Entropic | 4.97 | 5.35 | 14.3 | 1.86 | 0.95 | 0.81 | 0.016 (leapfrog (NextGEQ)) | 0.115 (min-scan) |
| Hybrid (bitmap + BP128) | 10.01 | 9.94 | 0.0 | 0.03 | 0.94 | 0.81 | 0.015 (decode + SvS) | 0.071 (min-scan) |
| Roaring | 11.72 | 11.10 | 0.0 | 0.08 | 1.34 | 1.51 | 0.007 (native) | 0.059 (native) |
| Pyramid | 10.35 | 9.95 | 0.0 | 0.34 | 0.71 | 0.69 | 0.005 (native) | 0.045 (native) |
| Roaring (roaring crate) | 11.72 | 11.10 | 0.0 | 0.16 | 5.65 | 5.73 | 0.016 (native) | 0.121 (min-scan) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (5.35 bits, 0.016 ms) → Loom (6.99 bits, 0.005 ms) → Uncompressed (32.00 bits, 0.004 ms)
* OR: Entropic (5.35 bits, 0.115 ms) → Interpolative (6.30 bits, 0.098 ms) → Loom (6.99 bits, 0.070 ms) → PEF (7.03 bits, 0.068 ms) → OptPFor (7.25 bits, 0.067 ms) → BP128 (bitpacking crate) (9.94 bits, 0.057 ms) → Pyramid (9.95 bits, 0.045 ms) → Uncompressed (32.00 bits, 0.038 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.014 |
| Uncompressed | AND | SvS + merge (branchless) | 0.021 |
| Uncompressed | AND | SvS + galloping | 0.008 |
| Uncompressed | AND | SvS + binary search | 0.054 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.011 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.012 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.005 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.004 |
| Uncompressed | AND | Small Adaptive | 0.014 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.012 |
| Uncompressed | OR | slices: heap | 0.098 |
| Uncompressed | OR | slices: fold (pairwise merge) | 0.038 |
| Uncompressed | OR | slices: bitmap | 0.102 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.008 |
| Uncompressed | AND | round-robin (LFTJ) | 0.011 |
| Uncompressed | AND | decode + SvS | 0.006 |
| Uncompressed | OR | heap | 0.069 |
| Uncompressed | OR | min-scan | 0.055 |
| Uncompressed | OR | bitset window (4096) | 0.111 |
| BP128 | AND | leapfrog (NextGEQ) | 0.018 |
| BP128 | AND | round-robin (LFTJ) | 0.021 |
| BP128 | AND | decode + SvS | 0.015 |
| BP128 | OR | heap | 0.087 |
| BP128 | OR | min-scan | 0.064 |
| BP128 | OR | bitset window (4096) | 0.125 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.016 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.018 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.009 |
| BP128 (bitpacking crate) | OR | heap | 0.079 |
| BP128 (bitpacking crate) | OR | min-scan | 0.057 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 0.119 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.019 |
| OptPFor | AND | round-robin (LFTJ) | 0.023 |
| OptPFor | AND | decode + SvS | 0.018 |
| OptPFor | OR | heap | 0.087 |
| OptPFor | OR | min-scan | 0.067 |
| OptPFor | OR | bitset window (4096) | 0.126 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.015 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.020 |
| Elias-Fano | AND | decode + SvS | 0.020 |
| Elias-Fano | OR | heap | 0.095 |
| Elias-Fano | OR | min-scan | 0.076 |
| Elias-Fano | OR | bitset window (4096) | 0.126 |
| PEF | AND | leapfrog (NextGEQ) | 0.013 |
| PEF | AND | round-robin (LFTJ) | 0.017 |
| PEF | AND | decode + SvS | 0.020 |
| PEF | OR | heap | 0.098 |
| PEF | OR | min-scan | 0.068 |
| PEF | OR | bitset window (4096) | 0.127 |
| Loom | AND | native | 0.005 |
| Loom | OR | native | 0.084 |
| Loom | AND | native, count only | 0.005 |
| Loom | OR | native, count only | 0.039 |
| Loom | AND | leapfrog (NextGEQ) | 0.012 |
| Loom | AND | round-robin (LFTJ) | 0.016 |
| Loom | AND | decode + SvS | 0.009 |
| Loom | OR | heap | 0.094 |
| Loom | OR | min-scan | 0.070 |
| Loom | OR | bitset window (4096) | 0.127 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.030 |
| Interpolative | AND | round-robin (LFTJ) | 0.038 |
| Interpolative | AND | decode + SvS | 0.049 |
| Interpolative | OR | heap | 0.121 |
| Interpolative | OR | min-scan | 0.098 |
| Interpolative | OR | bitset window (4096) | 0.159 |
| Entropic | AND | native | 0.016 |
| Entropic | OR | native | 0.225 |
| Entropic | AND | leapfrog (NextGEQ) | 0.016 |
| Entropic | AND | round-robin (LFTJ) | 0.022 |
| Entropic | AND | decode + SvS | 0.016 |
| Entropic | OR | heap | 0.118 |
| Entropic | OR | min-scan | 0.115 |
| Entropic | OR | bitset window (4096) | 0.158 |
| Hybrid (bitmap + BP128) | AND | native | 0.018 |
| Hybrid (bitmap + BP128) | OR | native | 0.092 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.018 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.022 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.015 |
| Hybrid (bitmap + BP128) | OR | heap | 0.092 |
| Hybrid (bitmap + BP128) | OR | min-scan | 0.071 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 0.125 |
| Roaring | AND | native | 0.007 |
| Roaring | OR | native | 0.059 |
| Roaring | AND | native, count only | 0.007 |
| Roaring | OR | native, count only | 0.058 |
| Roaring | AND | leapfrog (NextGEQ) | 0.009 |
| Roaring | AND | round-robin (LFTJ) | 0.013 |
| Roaring | AND | decode + SvS | 0.025 |
| Roaring | OR | heap | 0.096 |
| Roaring | OR | min-scan | 0.074 |
| Roaring | OR | bitset window (4096) | 0.119 |
| Pyramid | AND | native | 0.005 |
| Pyramid | OR | native | 0.045 |
| Pyramid | AND | native, count only | 0.005 |
| Pyramid | OR | native, count only | 0.033 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.011 |
| Pyramid | AND | round-robin (LFTJ) | 0.015 |
| Pyramid | AND | decode + SvS | 0.013 |
| Pyramid | OR | heap | 0.118 |
| Pyramid | OR | min-scan | 0.098 |
| Pyramid | OR | bitset window (4096) | 0.162 |
| Roaring (roaring crate) | AND | native | 0.016 |
| Roaring (roaring crate) | OR | native | 0.187 |
| Roaring (roaring crate) | AND | native, count only | 0.015 |
| Roaring (roaring crate) | OR | native, count only | 0.122 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.021 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.027 |
| Roaring (roaring crate) | AND | decode + SvS | 0.074 |
| Roaring (roaring crate) | OR | heap | 0.132 |
| Roaring (roaring crate) | OR | min-scan | 0.121 |
| Roaring (roaring crate) | OR | bitset window (4096) | 0.134 |
