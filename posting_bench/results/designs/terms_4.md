
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 4.00 terms, 875849 postings) |
| mean AND / OR result | 24 / 836333 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.05 | 0.23 | 0.23 | 0.029 (leapfrog (NextGEQ)) | — |
| BP128 | 10.07 | 7.61 | 0.0 | 0.07 | 1.28 | 0.94 | 0.036 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.61 | 0.0 | 0.03 | 0.57 | 0.41 | 0.033 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.88 | 0.0 | 0.21 | 1.49 | 1.20 | 0.044 (leapfrog (NextGEQ)) | — |
| Elias-Fano | 10.85 | 7.97 | 0.0 | 0.05 | 1.61 | 1.67 | 0.027 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.86 | 0.0 | 0.16 | 1.51 | 1.24 | 0.022 (leapfrog (NextGEQ)) | — |
| Loom | 6.86 | 4.83 | 0.0 | 0.48 | 0.55 | 0.35 | 0.012 (native) | — |
| Interpolative | 5.97 | 4.85 | 0.0 | 0.14 | 4.20 | 3.92 | 0.073 (leapfrog (NextGEQ)) | — |
| Entropic | 4.97 | 4.18 | 14.3 | 2.59 | 1.66 | 1.82 | 0.040 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.49 | 0.0 | 0.05 | 0.96 | 0.89 | 0.036 (leapfrog (NextGEQ)) | — |
| Roaring | 11.72 | 9.25 | 0.0 | 0.13 | 2.43 | 1.49 | 0.018 (native) | — |
| Pyramid | 10.35 | 6.04 | 0.0 | 0.27 | 0.96 | 0.40 | 0.011 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.25 | 0.0 | 0.23 | 7.44 | 6.40 | 0.049 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.18 bits, 0.040 ms) → Loom (4.83 bits, 0.012 ms) → Pyramid (6.04 bits, 0.011 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.397 |
| Uncompressed | AND | SvS + merge (branchless) | 1.627 |
| Uncompressed | AND | SvS + galloping | 0.031 |
| Uncompressed | AND | SvS + binary search | 0.128 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.043 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.730 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.724 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.578 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.030 |
| Uncompressed | AND | Small Adaptive | 0.037 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.125 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.029 |
| Uncompressed | AND | round-robin (LFTJ) | 0.131 |
| Uncompressed | AND | decode + SvS | 0.307 |
| BP128 | AND | leapfrog (NextGEQ) | 0.036 |
| BP128 | AND | round-robin (LFTJ) | 0.098 |
| BP128 | AND | decode + SvS | 0.609 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.033 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.085 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.376 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.044 |
| OptPFor | AND | round-robin (LFTJ) | 0.111 |
| OptPFor | AND | decode + SvS | 0.661 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.027 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.064 |
| Elias-Fano | AND | decode + SvS | 1.344 |
| PEF | AND | leapfrog (NextGEQ) | 0.022 |
| PEF | AND | round-robin (LFTJ) | 0.062 |
| PEF | AND | decode + SvS | 1.121 |
| Loom | AND | native | 0.012 |
| Loom | AND | native, count only | 0.010 |
| Loom | AND | leapfrog (NextGEQ) | 0.020 |
| Loom | AND | round-robin (LFTJ) | 0.053 |
| Loom | AND | decode + SvS | 0.279 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.073 |
| Interpolative | AND | round-robin (LFTJ) | 0.316 |
| Interpolative | AND | decode + SvS | 3.799 |
| Entropic | AND | native | 0.041 |
| Entropic | AND | leapfrog (NextGEQ) | 0.040 |
| Entropic | AND | round-robin (LFTJ) | 0.143 |
| Entropic | AND | decode + SvS | 1.127 |
| Hybrid (bitmap + BP128) | AND | native | 0.036 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.036 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.084 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 1.114 |
| Roaring | AND | native | 0.018 |
| Roaring | AND | native, count only | 0.019 |
| Roaring | AND | leapfrog (NextGEQ) | 0.020 |
| Roaring | AND | round-robin (LFTJ) | 0.063 |
| Roaring | AND | decode + SvS | 0.512 |
| Pyramid | AND | native | 0.011 |
| Pyramid | AND | native, count only | 0.011 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.019 |
| Pyramid | AND | round-robin (LFTJ) | 0.050 |
| Pyramid | AND | decode + SvS | 0.247 |
| Roaring (roaring crate) | AND | native | 0.081 |
| Roaring (roaring crate) | AND | native, count only | 0.077 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.049 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.095 |
| Roaring (roaring crate) | AND | decode + SvS | 5.258 |
