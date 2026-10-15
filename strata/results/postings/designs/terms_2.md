
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 2.00 terms, 401527 postings) |
| mean AND / OR result | 743 / 400784 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.09 | 0.08 | 0.036 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.35 | 0.0 | 0.03 | 0.88 | 0.76 | 0.100 (round-robin (LFTJ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.35 | 0.0 | 0.01 | 0.44 | 0.29 | 0.075 (round-robin (LFTJ)) | — |
| OptPFor | 7.13 | 5.71 | 0.0 | 0.14 | 1.11 | 0.88 | 0.101 (round-robin (LFTJ)) | — |
| Elias-Fano | 10.85 | 7.42 | 0.0 | 0.05 | 1.15 | 1.14 | 0.060 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.62 | 0.0 | 0.18 | 1.11 | 0.99 | 0.047 (leapfrog (NextGEQ)) | — |
| Loom | 6.86 | 4.58 | 0.0 | 0.49 | 0.36 | 0.23 | 0.018 (native) | — |
| Interpolative | 5.97 | 4.66 | 0.0 | 0.09 | 3.18 | 3.11 | 0.217 (round-robin (LFTJ)) | — |
| Entropic | 4.97 | 4.02 | 14.3 | 1.86 | 0.95 | 0.77 | 0.097 (native) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.22 | 0.0 | 0.03 | 0.92 | 0.87 | 0.060 (native) | — |
| Roaring | 11.72 | 9.07 | 0.0 | 0.08 | 1.34 | 0.87 | 0.019 (native) | — |
| Pyramid | 10.35 | 5.73 | 0.0 | 0.34 | 0.71 | 0.40 | 0.017 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.07 | 0.0 | 0.16 | 5.65 | 5.15 | 0.033 (native) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.02 bits, 0.097 ms) → Loom (4.58 bits, 0.018 ms) → Pyramid (5.73 bits, 0.017 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.356 |
| Uncompressed | AND | SvS + merge (branchless) | 0.555 |
| Uncompressed | AND | SvS + galloping | 0.039 |
| Uncompressed | AND | SvS + binary search | 0.214 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.059 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.283 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.194 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.036 |
| Uncompressed | AND | Small Adaptive | 0.051 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.048 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.045 |
| Uncompressed | AND | round-robin (LFTJ) | 0.046 |
| Uncompressed | AND | decode + SvS | 0.061 |
| BP128 | AND | leapfrog (NextGEQ) | 0.100 |
| BP128 | AND | round-robin (LFTJ) | 0.100 |
| BP128 | AND | decode + SvS | 0.338 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.075 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.075 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.150 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.101 |
| OptPFor | AND | round-robin (LFTJ) | 0.101 |
| OptPFor | AND | decode + SvS | 0.379 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.060 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.062 |
| Elias-Fano | AND | decode + SvS | 0.487 |
| PEF | AND | leapfrog (NextGEQ) | 0.047 |
| PEF | AND | round-robin (LFTJ) | 0.048 |
| PEF | AND | decode + SvS | 0.421 |
| Loom | AND | native | 0.018 |
| Loom | AND | native, count only | 0.018 |
| Loom | AND | leapfrog (NextGEQ) | 0.043 |
| Loom | AND | round-robin (LFTJ) | 0.044 |
| Loom | AND | decode + SvS | 0.114 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.221 |
| Interpolative | AND | round-robin (LFTJ) | 0.217 |
| Interpolative | AND | decode + SvS | 1.297 |
| Entropic | AND | native | 0.097 |
| Entropic | AND | leapfrog (NextGEQ) | 0.103 |
| Entropic | AND | round-robin (LFTJ) | 0.107 |
| Entropic | AND | decode + SvS | 0.335 |
| Hybrid (bitmap + BP128) | AND | native | 0.060 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.068 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.070 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.430 |
| Roaring | AND | native | 0.019 |
| Roaring | AND | native, count only | 0.019 |
| Roaring | AND | leapfrog (NextGEQ) | 0.030 |
| Roaring | AND | round-robin (LFTJ) | 0.031 |
| Roaring | AND | decode + SvS | 0.202 |
| Pyramid | AND | native | 0.017 |
| Pyramid | AND | native, count only | 0.016 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.051 |
| Pyramid | AND | round-robin (LFTJ) | 0.052 |
| Pyramid | AND | decode + SvS | 0.168 |
| Roaring (roaring crate) | AND | native | 0.033 |
| Roaring (roaring crate) | AND | native, count only | 0.031 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.066 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.067 |
| Roaring (roaring crate) | AND | decode + SvS | 1.716 |
