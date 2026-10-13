
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 3.00 terms, 684896 postings) |
| mean AND / OR result | 111 / 669995 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.06 | 0.41 | 0.28 | 0.041 (leapfrog (NextGEQ)) | — |
| BP128 | 10.07 | 7.51 | 0.0 | 0.06 | 0.94 | 0.69 | 0.059 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.51 | 0.0 | 0.03 | 0.64 | 0.40 | 0.050 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.81 | 0.0 | 0.21 | 1.27 | 0.76 | 0.057 (leapfrog (NextGEQ)) | — |
| Elias-Fano | 10.85 | 7.77 | 0.0 | 0.05 | 1.60 | 1.57 | 0.044 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.77 | 0.0 | 0.16 | 1.60 | 1.40 | 0.038 (leapfrog (NextGEQ)) | — |
| Loom | 6.86 | 4.73 | 0.0 | 0.49 | 0.57 | 0.30 | 0.018 (native) | — |
| Interpolative | 5.97 | 4.78 | 0.0 | 0.16 | 5.16 | 4.62 | 0.144 (leapfrog (NextGEQ)) | — |
| Entropic | 4.97 | 4.12 | 14.3 | 2.78 | 1.69 | 1.30 | 0.065 (native) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.39 | 0.0 | 0.05 | 1.20 | 0.94 | 0.059 (native) | — |
| Roaring | 11.72 | 9.17 | 0.0 | 0.14 | 2.75 | 1.74 | 0.029 (native) | — |
| Pyramid | 10.35 | 5.90 | 0.0 | 0.29 | 0.97 | 0.40 | 0.017 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.17 | 0.0 | 0.23 | 7.37 | 6.58 | 0.071 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.12 bits, 0.065 ms) → Loom (4.73 bits, 0.018 ms) → Pyramid (5.90 bits, 0.017 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.450 |
| Uncompressed | AND | SvS + merge (branchless) | 1.799 |
| Uncompressed | AND | SvS + galloping | 0.044 |
| Uncompressed | AND | SvS + binary search | 0.220 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.070 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.719 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.776 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.602 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.053 |
| Uncompressed | AND | Small Adaptive | 0.054 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.117 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.041 |
| Uncompressed | AND | round-robin (LFTJ) | 0.102 |
| Uncompressed | AND | decode + SvS | 0.241 |
| BP128 | AND | leapfrog (NextGEQ) | 0.059 |
| BP128 | AND | round-robin (LFTJ) | 0.108 |
| BP128 | AND | decode + SvS | 0.465 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.050 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.100 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.319 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.057 |
| OptPFor | AND | round-robin (LFTJ) | 0.114 |
| OptPFor | AND | decode + SvS | 0.551 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.044 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.074 |
| Elias-Fano | AND | decode + SvS | 1.135 |
| PEF | AND | leapfrog (NextGEQ) | 0.038 |
| PEF | AND | round-robin (LFTJ) | 0.089 |
| PEF | AND | decode + SvS | 0.942 |
| Loom | AND | native | 0.018 |
| Loom | AND | native, count only | 0.017 |
| Loom | AND | leapfrog (NextGEQ) | 0.031 |
| Loom | AND | round-robin (LFTJ) | 0.055 |
| Loom | AND | decode + SvS | 0.256 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.144 |
| Interpolative | AND | round-robin (LFTJ) | 0.372 |
| Interpolative | AND | decode + SvS | 3.043 |
| Entropic | AND | native | 0.065 |
| Entropic | AND | leapfrog (NextGEQ) | 0.067 |
| Entropic | AND | round-robin (LFTJ) | 0.199 |
| Entropic | AND | decode + SvS | 1.006 |
| Hybrid (bitmap + BP128) | AND | native | 0.059 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.062 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.098 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.976 |
| Roaring | AND | native | 0.029 |
| Roaring | AND | native, count only | 0.028 |
| Roaring | AND | leapfrog (NextGEQ) | 0.036 |
| Roaring | AND | round-robin (LFTJ) | 0.056 |
| Roaring | AND | decode + SvS | 0.421 |
| Pyramid | AND | native | 0.017 |
| Pyramid | AND | native, count only | 0.015 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.032 |
| Pyramid | AND | round-robin (LFTJ) | 0.054 |
| Pyramid | AND | decode + SvS | 0.236 |
| Roaring (roaring crate) | AND | native | 0.100 |
| Roaring (roaring crate) | AND | native, count only | 0.091 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.071 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.100 |
| Roaring (roaring crate) | AND | decode + SvS | 3.934 |
