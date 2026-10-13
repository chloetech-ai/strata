
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 2.00 terms, 401527 postings) |
| mean AND / OR result | 743 / 400784 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.05 | 0.26 | 0.25 | 0.089 (SvS + galloping) | — |
| BP128 | 10.07 | 7.35 | 0.0 | 0.06 | 0.92 | 0.70 | 0.134 (round-robin (LFTJ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.35 | 0.0 | 0.03 | 0.69 | 0.48 | 0.118 (round-robin (LFTJ)) | — |
| OptPFor | 7.13 | 5.71 | 0.0 | 0.27 | 1.53 | 1.29 | 0.132 (round-robin (LFTJ)) | — |
| Elias-Fano | 10.85 | 7.42 | 0.0 | 0.05 | 1.76 | 1.89 | 0.071 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.62 | 0.0 | 0.17 | 1.77 | 1.46 | 0.078 (leapfrog (NextGEQ)) | — |
| Loom | 6.86 | 4.58 | 0.0 | 0.50 | 0.52 | 0.32 | 0.029 (native) | — |
| Interpolative | 5.97 | 4.66 | 0.0 | 0.17 | 4.55 | 4.27 | 0.299 (leapfrog (NextGEQ)) | — |
| Entropic | 4.97 | 4.02 | 14.3 | 2.84 | 1.71 | 1.51 | 0.124 (native) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.22 | 0.0 | 0.05 | 0.98 | 0.93 | 0.074 (native) | — |
| Roaring | 11.72 | 9.07 | 0.0 | 0.12 | 2.51 | 1.53 | 0.026 (native) | — |
| Pyramid | 10.35 | 5.73 | 0.0 | 0.27 | 0.92 | 0.38 | 0.019 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.07 | 0.0 | 0.29 | 7.70 | 8.38 | 0.062 (native) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.02 bits, 0.124 ms) → Loom (4.58 bits, 0.029 ms) → Pyramid (5.73 bits, 0.019 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.324 |
| Uncompressed | AND | SvS + merge (branchless) | 1.335 |
| Uncompressed | AND | SvS + galloping | 0.089 |
| Uncompressed | AND | SvS + binary search | 0.457 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.165 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.590 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.569 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.484 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.110 |
| Uncompressed | AND | Small Adaptive | 0.112 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.110 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.106 |
| Uncompressed | AND | round-robin (LFTJ) | 0.111 |
| Uncompressed | AND | decode + SvS | 0.218 |
| BP128 | AND | leapfrog (NextGEQ) | 0.134 |
| BP128 | AND | round-robin (LFTJ) | 0.134 |
| BP128 | AND | decode + SvS | 0.386 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.122 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.118 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.295 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.138 |
| OptPFor | AND | round-robin (LFTJ) | 0.132 |
| OptPFor | AND | decode + SvS | 0.422 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.071 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.078 |
| Elias-Fano | AND | decode + SvS | 0.912 |
| PEF | AND | leapfrog (NextGEQ) | 0.078 |
| PEF | AND | round-robin (LFTJ) | 0.085 |
| PEF | AND | decode + SvS | 0.623 |
| Loom | AND | native | 0.029 |
| Loom | AND | native, count only | 0.030 |
| Loom | AND | leapfrog (NextGEQ) | 0.052 |
| Loom | AND | round-robin (LFTJ) | 0.057 |
| Loom | AND | decode + SvS | 0.209 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.299 |
| Interpolative | AND | round-robin (LFTJ) | 0.321 |
| Interpolative | AND | decode + SvS | 2.036 |
| Entropic | AND | native | 0.124 |
| Entropic | AND | leapfrog (NextGEQ) | 0.126 |
| Entropic | AND | round-robin (LFTJ) | 0.145 |
| Entropic | AND | decode + SvS | 0.594 |
| Hybrid (bitmap + BP128) | AND | native | 0.074 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.078 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.089 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.543 |
| Roaring | AND | native | 0.026 |
| Roaring | AND | native, count only | 0.029 |
| Roaring | AND | leapfrog (NextGEQ) | 0.045 |
| Roaring | AND | round-robin (LFTJ) | 0.047 |
| Roaring | AND | decode + SvS | 0.262 |
| Pyramid | AND | native | 0.019 |
| Pyramid | AND | native, count only | 0.017 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.052 |
| Pyramid | AND | round-robin (LFTJ) | 0.055 |
| Pyramid | AND | decode + SvS | 0.182 |
| Roaring (roaring crate) | AND | native | 0.062 |
| Roaring (roaring crate) | AND | native, count only | 0.060 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.128 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.106 |
| Roaring (roaring crate) | AND | decode + SvS | 2.486 |
