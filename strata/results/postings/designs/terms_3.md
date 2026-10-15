
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 3.00 terms, 684896 postings) |
| mean AND / OR result | 111 / 669995 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.08 | 0.021 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.51 | 0.0 | 0.03 | 0.89 | 0.76 | 0.051 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.51 | 0.0 | 0.01 | 0.44 | 0.30 | 0.040 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.81 | 0.0 | 0.14 | 1.14 | 0.88 | 0.052 (leapfrog (NextGEQ)) | — |
| Elias-Fano | 10.85 | 7.77 | 0.0 | 0.05 | 1.15 | 1.13 | 0.037 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.77 | 0.0 | 0.18 | 1.11 | 1.00 | 0.029 (leapfrog (NextGEQ)) | — |
| Loom | 6.86 | 4.73 | 0.0 | 0.49 | 0.35 | 0.23 | 0.011 (native) | — |
| Interpolative | 5.97 | 4.78 | 0.0 | 0.09 | 3.20 | 3.13 | 0.100 (leapfrog (NextGEQ)) | — |
| Entropic | 4.97 | 4.12 | 14.3 | 1.87 | 0.95 | 0.78 | 0.050 (native) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.39 | 0.0 | 0.03 | 0.94 | 0.86 | 0.048 (leapfrog (NextGEQ)) | — |
| Roaring | 11.72 | 9.17 | 0.0 | 0.08 | 1.35 | 0.89 | 0.017 (native) | — |
| Pyramid | 10.35 | 5.90 | 0.0 | 0.34 | 0.71 | 0.42 | 0.012 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.17 | 0.0 | 0.16 | 5.65 | 5.17 | 0.050 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.12 bits, 0.050 ms) → Loom (4.73 bits, 0.011 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.497 |
| Uncompressed | AND | SvS + merge (branchless) | 0.692 |
| Uncompressed | AND | SvS + galloping | 0.023 |
| Uncompressed | AND | SvS + binary search | 0.134 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.033 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.321 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.285 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.021 |
| Uncompressed | AND | Small Adaptive | 0.032 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.052 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.023 |
| Uncompressed | AND | round-robin (LFTJ) | 0.048 |
| Uncompressed | AND | decode + SvS | 0.063 |
| BP128 | AND | leapfrog (NextGEQ) | 0.051 |
| BP128 | AND | round-robin (LFTJ) | 0.106 |
| BP128 | AND | decode + SvS | 0.534 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.040 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.070 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.217 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.052 |
| OptPFor | AND | round-robin (LFTJ) | 0.110 |
| OptPFor | AND | decode + SvS | 0.606 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.037 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.060 |
| Elias-Fano | AND | decode + SvS | 0.791 |
| PEF | AND | leapfrog (NextGEQ) | 0.029 |
| PEF | AND | round-robin (LFTJ) | 0.048 |
| PEF | AND | decode + SvS | 0.680 |
| Loom | AND | native | 0.011 |
| Loom | AND | native, count only | 0.011 |
| Loom | AND | leapfrog (NextGEQ) | 0.027 |
| Loom | AND | round-robin (LFTJ) | 0.044 |
| Loom | AND | decode + SvS | 0.158 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.100 |
| Interpolative | AND | round-robin (LFTJ) | 0.273 |
| Interpolative | AND | decode + SvS | 2.173 |
| Entropic | AND | native | 0.050 |
| Entropic | AND | leapfrog (NextGEQ) | 0.051 |
| Entropic | AND | round-robin (LFTJ) | 0.132 |
| Entropic | AND | decode + SvS | 0.535 |
| Hybrid (bitmap + BP128) | AND | native | 0.048 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.048 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.070 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.695 |
| Roaring | AND | native | 0.017 |
| Roaring | AND | native, count only | 0.017 |
| Roaring | AND | leapfrog (NextGEQ) | 0.020 |
| Roaring | AND | round-robin (LFTJ) | 0.031 |
| Roaring | AND | decode + SvS | 0.324 |
| Pyramid | AND | native | 0.012 |
| Pyramid | AND | native, count only | 0.012 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.032 |
| Pyramid | AND | round-robin (LFTJ) | 0.053 |
| Pyramid | AND | decode + SvS | 0.242 |
| Roaring (roaring crate) | AND | native | 0.058 |
| Roaring (roaring crate) | AND | native, count only | 0.057 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.050 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.065 |
| Roaring (roaring crate) | AND | decode + SvS | 2.904 |
