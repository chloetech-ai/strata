
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 6.00 terms, 735851 postings) |
| mean AND / OR result | 2 / 686811 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.06 | 0.27 | 0.21 | 0.011 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.80 | 0.0 | 0.05 | 0.78 | 0.61 | 0.019 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.80 | 0.0 | 0.03 | 0.56 | 0.38 | 0.017 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.98 | 0.0 | 0.22 | 1.26 | 0.89 | 0.020 (leapfrog (NextGEQ)) | — |
| Elias-Fano | 10.85 | 8.32 | 0.0 | 0.05 | 1.59 | 1.55 | 0.016 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 5.03 | 0.0 | 0.16 | 1.61 | 1.48 | 0.014 (leapfrog (NextGEQ)) | — |
| Loom | 6.86 | 4.99 | 0.0 | 0.42 | 0.46 | 0.28 | 0.007 (native) | — |
| Interpolative | 5.97 | 4.96 | 0.0 | 0.14 | 4.19 | 3.96 | 0.039 (leapfrog (NextGEQ)) | — |
| Entropic | 4.97 | 4.27 | 14.3 | 2.61 | 1.60 | 1.39 | 0.024 (native) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.69 | 0.0 | 0.07 | 1.05 | 0.94 | 0.021 (leapfrog (NextGEQ)) | — |
| Roaring | 11.72 | 9.43 | 0.0 | 0.14 | 2.49 | 1.91 | 0.013 (native) | — |
| Pyramid | 10.35 | 6.40 | 0.0 | 0.29 | 1.04 | 0.48 | 0.009 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.43 | 0.0 | 0.24 | 7.99 | 6.87 | 0.039 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.27 bits, 0.024 ms) → Loom (4.99 bits, 0.007 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.273 |
| Uncompressed | AND | SvS + merge (branchless) | 1.114 |
| Uncompressed | AND | SvS + galloping | 0.014 |
| Uncompressed | AND | SvS + binary search | 0.066 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.023 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.452 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.514 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.321 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.011 |
| Uncompressed | AND | Small Adaptive | 0.023 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.087 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.013 |
| Uncompressed | AND | round-robin (LFTJ) | 0.085 |
| Uncompressed | AND | decode + SvS | 0.224 |
| BP128 | AND | leapfrog (NextGEQ) | 0.019 |
| BP128 | AND | round-robin (LFTJ) | 0.077 |
| BP128 | AND | decode + SvS | 0.464 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.017 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.065 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.300 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.020 |
| OptPFor | AND | round-robin (LFTJ) | 0.084 |
| OptPFor | AND | decode + SvS | 0.579 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.016 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.057 |
| Elias-Fano | AND | decode + SvS | 1.132 |
| PEF | AND | leapfrog (NextGEQ) | 0.014 |
| PEF | AND | round-robin (LFTJ) | 0.053 |
| PEF | AND | decode + SvS | 0.926 |
| Loom | AND | native | 0.007 |
| Loom | AND | native, count only | 0.007 |
| Loom | AND | leapfrog (NextGEQ) | 0.011 |
| Loom | AND | round-robin (LFTJ) | 0.042 |
| Loom | AND | decode + SvS | 0.218 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.039 |
| Interpolative | AND | round-robin (LFTJ) | 0.236 |
| Interpolative | AND | decode + SvS | 3.300 |
| Entropic | AND | native | 0.024 |
| Entropic | AND | leapfrog (NextGEQ) | 0.026 |
| Entropic | AND | round-robin (LFTJ) | 0.138 |
| Entropic | AND | decode + SvS | 1.010 |
| Hybrid (bitmap + BP128) | AND | native | 0.029 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.021 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.079 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.937 |
| Roaring | AND | native | 0.013 |
| Roaring | AND | native, count only | 0.011 |
| Roaring | AND | leapfrog (NextGEQ) | 0.015 |
| Roaring | AND | round-robin (LFTJ) | 0.053 |
| Roaring | AND | decode + SvS | 0.535 |
| Pyramid | AND | native | 0.009 |
| Pyramid | AND | native, count only | 0.008 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.012 |
| Pyramid | AND | round-robin (LFTJ) | 0.044 |
| Pyramid | AND | decode + SvS | 0.222 |
| Roaring (roaring crate) | AND | native | 0.052 |
| Roaring (roaring crate) | AND | native, count only | 0.051 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.039 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.097 |
| Roaring (roaring crate) | AND | decode + SvS | 4.338 |
