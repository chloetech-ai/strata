
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 500 (mean 2.79 terms, 13800 postings) |
| mean AND / OR result | 81 / 13274 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.05 | 0.24 | 0.16 | 0.009 (SvS + adaptive (SIMD / gallop @64x)) | 0.061 (slices: fold (pairwise merge)) |
| BP128 | 10.07 | 9.94 | 0.0 | 0.05 | 0.86 | 0.63 | 0.020 (decode + SvS) | 0.092 (min-scan) |
| BP128 (bitpacking crate) | 10.07 | 9.94 | 0.0 | 0.04 | 0.81 | 0.57 | 0.014 (decode + SvS) | 0.088 (min-scan) |
| OptPFor | 7.13 | 7.25 | 0.0 | 0.20 | 1.15 | 0.83 | 0.021 (leapfrog (NextGEQ)) | 0.087 (min-scan) |
| Elias-Fano | 10.85 | 12.54 | 0.0 | 0.05 | 1.73 | 1.44 | 0.016 (leapfrog (NextGEQ)) | 0.088 (min-scan) |
| PEF | 7.04 | 7.03 | 0.0 | 0.16 | 1.64 | 1.58 | 0.015 (leapfrog (NextGEQ)) | 0.092 (min-scan) |
| Loom | 6.86 | 6.99 | 0.0 | 0.41 | 0.51 | 0.36 | 0.007 (native) | 0.073 (native) |
| Interpolative | 5.97 | 6.30 | 0.0 | 0.14 | 4.31 | 4.26 | 0.040 (leapfrog (NextGEQ)) | 0.126 (min-scan) |
| Entropic | 4.97 | 5.35 | 14.3 | 2.66 | 1.81 | 1.77 | 0.022 (leapfrog (NextGEQ)) | 0.110 (min-scan) |
| Hybrid (bitmap + BP128) | 10.01 | 9.94 | 0.0 | 0.05 | 0.96 | 0.70 | 0.018 (decode + SvS) | 0.086 (min-scan) |
| Roaring | 11.72 | 11.10 | 0.0 | 0.13 | 2.36 | 2.52 | 0.010 (native) | 0.100 (min-scan) |
| Pyramid | 10.35 | 9.95 | 0.0 | 0.27 | 0.95 | 0.87 | 0.008 (native) | 0.059 (native) |
| Roaring (roaring crate) | 11.72 | 11.10 | 0.0 | 0.26 | 7.80 | 7.61 | 0.025 (native) | 0.166 (min-scan) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (5.35 bits, 0.022 ms) → Loom (6.99 bits, 0.007 ms)
* OR: Entropic (5.35 bits, 0.110 ms) → Loom (6.99 bits, 0.073 ms) → Pyramid (9.95 bits, 0.059 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.017 |
| Uncompressed | AND | SvS + merge (branchless) | 0.034 |
| Uncompressed | AND | SvS + galloping | 0.013 |
| Uncompressed | AND | SvS + binary search | 0.070 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.019 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.014 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.014 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.012 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.009 |
| Uncompressed | AND | Small Adaptive | 0.019 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.019 |
| Uncompressed | OR | slices: heap | 0.168 |
| Uncompressed | OR | slices: fold (pairwise merge) | 0.061 |
| Uncompressed | OR | slices: bitmap | 0.184 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.013 |
| Uncompressed | AND | round-robin (LFTJ) | 0.017 |
| Uncompressed | AND | decode + SvS | 0.012 |
| Uncompressed | OR | heap | 0.097 |
| Uncompressed | OR | min-scan | 0.079 |
| Uncompressed | OR | bitset window (4096) | 0.148 |
| BP128 | AND | leapfrog (NextGEQ) | 0.021 |
| BP128 | AND | round-robin (LFTJ) | 0.026 |
| BP128 | AND | decode + SvS | 0.020 |
| BP128 | OR | heap | 0.123 |
| BP128 | OR | min-scan | 0.092 |
| BP128 | OR | bitset window (4096) | 0.160 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.020 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.022 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.014 |
| BP128 (bitpacking crate) | OR | heap | 0.106 |
| BP128 (bitpacking crate) | OR | min-scan | 0.088 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 0.157 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.021 |
| OptPFor | AND | round-robin (LFTJ) | 0.027 |
| OptPFor | AND | decode + SvS | 0.021 |
| OptPFor | OR | heap | 0.115 |
| OptPFor | OR | min-scan | 0.087 |
| OptPFor | OR | bitset window (4096) | 0.154 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.016 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.022 |
| Elias-Fano | AND | decode + SvS | 0.031 |
| Elias-Fano | OR | heap | 0.117 |
| Elias-Fano | OR | min-scan | 0.088 |
| Elias-Fano | OR | bitset window (4096) | 0.158 |
| PEF | AND | leapfrog (NextGEQ) | 0.015 |
| PEF | AND | round-robin (LFTJ) | 0.021 |
| PEF | AND | decode + SvS | 0.030 |
| PEF | OR | heap | 0.123 |
| PEF | OR | min-scan | 0.092 |
| PEF | OR | bitset window (4096) | 0.175 |
| Loom | AND | native | 0.007 |
| Loom | OR | native | 0.073 |
| Loom | AND | native, count only | 0.007 |
| Loom | OR | native, count only | 0.039 |
| Loom | AND | leapfrog (NextGEQ) | 0.013 |
| Loom | AND | round-robin (LFTJ) | 0.017 |
| Loom | AND | decode + SvS | 0.013 |
| Loom | OR | heap | 0.122 |
| Loom | OR | min-scan | 0.094 |
| Loom | OR | bitset window (4096) | 0.174 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.040 |
| Interpolative | AND | round-robin (LFTJ) | 0.047 |
| Interpolative | AND | decode + SvS | 0.068 |
| Interpolative | OR | heap | 0.159 |
| Interpolative | OR | min-scan | 0.126 |
| Interpolative | OR | bitset window (4096) | 0.230 |
| Entropic | AND | native | 0.025 |
| Entropic | OR | native | 0.244 |
| Entropic | AND | leapfrog (NextGEQ) | 0.022 |
| Entropic | AND | round-robin (LFTJ) | 0.030 |
| Entropic | AND | decode + SvS | 0.027 |
| Entropic | OR | heap | 0.143 |
| Entropic | OR | min-scan | 0.110 |
| Entropic | OR | bitset window (4096) | 0.235 |
| Hybrid (bitmap + BP128) | AND | native | 0.023 |
| Hybrid (bitmap + BP128) | OR | native | 0.125 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.021 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.025 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.018 |
| Hybrid (bitmap + BP128) | OR | heap | 0.126 |
| Hybrid (bitmap + BP128) | OR | min-scan | 0.086 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 0.167 |
| Roaring | AND | native | 0.010 |
| Roaring | OR | native | 0.111 |
| Roaring | AND | native, count only | 0.010 |
| Roaring | OR | native, count only | 0.104 |
| Roaring | AND | leapfrog (NextGEQ) | 0.013 |
| Roaring | AND | round-robin (LFTJ) | 0.020 |
| Roaring | AND | decode + SvS | 0.040 |
| Roaring | OR | heap | 0.140 |
| Roaring | OR | min-scan | 0.100 |
| Roaring | OR | bitset window (4096) | 0.176 |
| Pyramid | AND | native | 0.008 |
| Pyramid | OR | native | 0.059 |
| Pyramid | AND | native, count only | 0.007 |
| Pyramid | OR | native, count only | 0.044 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.013 |
| Pyramid | AND | round-robin (LFTJ) | 0.018 |
| Pyramid | AND | decode + SvS | 0.019 |
| Pyramid | OR | heap | 0.142 |
| Pyramid | OR | min-scan | 0.127 |
| Pyramid | OR | bitset window (4096) | 0.327 |
| Roaring (roaring crate) | AND | native | 0.025 |
| Roaring (roaring crate) | OR | native | 0.252 |
| Roaring (roaring crate) | AND | native, count only | 0.024 |
| Roaring (roaring crate) | OR | native, count only | 0.165 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.032 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.035 |
| Roaring (roaring crate) | AND | decode + SvS | 0.105 |
| Roaring (roaring crate) | OR | heap | 0.201 |
| Roaring (roaring crate) | OR | min-scan | 0.166 |
| Roaring (roaring crate) | OR | bitset window (4096) | 0.279 |
