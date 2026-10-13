
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 5.00 terms, 763061 postings) |
| mean AND / OR result | 6 / 720237 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.05 | 0.25 | 0.23 | 0.017 (leapfrog (NextGEQ)) | — |
| BP128 | 10.07 | 7.74 | 0.0 | 0.05 | 0.94 | 0.67 | 0.027 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.74 | 0.0 | 0.03 | 0.56 | 0.43 | 0.023 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.95 | 0.0 | 0.21 | 1.19 | 0.71 | 0.030 (leapfrog (NextGEQ)) | — |
| Elias-Fano | 10.85 | 8.21 | 0.0 | 0.05 | 1.58 | 1.51 | 0.020 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.98 | 0.0 | 0.15 | 1.57 | 1.25 | 0.017 (leapfrog (NextGEQ)) | — |
| Loom | 6.86 | 4.94 | 0.0 | 0.43 | 0.46 | 0.27 | 0.008 (native) | — |
| Interpolative | 5.97 | 4.93 | 0.0 | 0.18 | 4.62 | 3.94 | 0.054 (leapfrog (NextGEQ)) | — |
| Entropic | 4.97 | 4.24 | 14.3 | 2.46 | 1.62 | 1.35 | 0.031 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.63 | 0.0 | 0.05 | 1.00 | 0.79 | 0.025 (native) | — |
| Roaring | 11.72 | 9.36 | 0.0 | 0.12 | 2.51 | 2.10 | 0.018 (leapfrog (NextGEQ)) | — |
| Pyramid | 10.35 | 6.27 | 0.0 | 0.30 | 1.06 | 0.47 | 0.010 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.36 | 0.0 | 0.35 | 7.56 | 6.73 | 0.048 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.24 bits, 0.031 ms) → Loom (4.94 bits, 0.008 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.318 |
| Uncompressed | AND | SvS + merge (branchless) | 1.254 |
| Uncompressed | AND | SvS + galloping | 0.019 |
| Uncompressed | AND | SvS + binary search | 0.094 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.031 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.515 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.518 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.360 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.018 |
| Uncompressed | AND | Small Adaptive | 0.028 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.099 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.017 |
| Uncompressed | AND | round-robin (LFTJ) | 0.097 |
| Uncompressed | AND | decode + SvS | 0.238 |
| BP128 | AND | leapfrog (NextGEQ) | 0.027 |
| BP128 | AND | round-robin (LFTJ) | 0.092 |
| BP128 | AND | decode + SvS | 0.512 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.023 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.073 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.322 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.030 |
| OptPFor | AND | round-robin (LFTJ) | 0.100 |
| OptPFor | AND | decode + SvS | 0.587 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.020 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.062 |
| Elias-Fano | AND | decode + SvS | 1.184 |
| PEF | AND | leapfrog (NextGEQ) | 0.017 |
| PEF | AND | round-robin (LFTJ) | 0.054 |
| PEF | AND | decode + SvS | 0.942 |
| Loom | AND | native | 0.008 |
| Loom | AND | native, count only | 0.009 |
| Loom | AND | leapfrog (NextGEQ) | 0.015 |
| Loom | AND | round-robin (LFTJ) | 0.051 |
| Loom | AND | decode + SvS | 0.226 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.054 |
| Interpolative | AND | round-robin (LFTJ) | 0.281 |
| Interpolative | AND | decode + SvS | 3.364 |
| Entropic | AND | native | 0.032 |
| Entropic | AND | leapfrog (NextGEQ) | 0.031 |
| Entropic | AND | round-robin (LFTJ) | 0.156 |
| Entropic | AND | decode + SvS | 1.044 |
| Hybrid (bitmap + BP128) | AND | native | 0.025 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.027 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.086 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.964 |
| Roaring | AND | native | 0.019 |
| Roaring | AND | native, count only | 0.016 |
| Roaring | AND | leapfrog (NextGEQ) | 0.018 |
| Roaring | AND | round-robin (LFTJ) | 0.061 |
| Roaring | AND | decode + SvS | 0.558 |
| Pyramid | AND | native | 0.010 |
| Pyramid | AND | native, count only | 0.011 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.016 |
| Pyramid | AND | round-robin (LFTJ) | 0.054 |
| Pyramid | AND | decode + SvS | 0.222 |
| Roaring (roaring crate) | AND | native | 0.073 |
| Roaring (roaring crate) | AND | native, count only | 0.074 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.048 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.106 |
| Roaring (roaring crate) | AND | decode + SvS | 4.319 |
