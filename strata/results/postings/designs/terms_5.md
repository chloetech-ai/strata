
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 5.00 terms, 763061 postings) |
| mean AND / OR result | 6 / 720237 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.08 | 0.008 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.74 | 0.0 | 0.03 | 0.89 | 0.77 | 0.022 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.74 | 0.0 | 0.01 | 0.44 | 0.30 | 0.018 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.95 | 0.0 | 0.14 | 1.14 | 0.89 | 0.024 (leapfrog (NextGEQ)) | — |
| Elias-Fano | 10.85 | 8.21 | 0.0 | 0.05 | 1.14 | 1.13 | 0.019 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.98 | 0.0 | 0.18 | 1.12 | 1.01 | 0.015 (leapfrog (NextGEQ)) | — |
| Loom | 6.86 | 4.94 | 0.0 | 0.49 | 0.36 | 0.24 | 0.006 (native) | — |
| Interpolative | 5.97 | 4.93 | 0.0 | 0.09 | 3.22 | 3.14 | 0.043 (leapfrog (NextGEQ)) | — |
| Entropic | 4.97 | 4.24 | 14.3 | 1.87 | 0.95 | 0.78 | 0.022 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.63 | 0.0 | 0.03 | 0.94 | 0.85 | 0.022 (native) | — |
| Roaring | 11.72 | 9.36 | 0.0 | 0.08 | 1.35 | 0.96 | 0.010 (native) | — |
| Pyramid | 10.35 | 6.27 | 0.0 | 0.34 | 0.71 | 0.45 | 0.007 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.36 | 0.0 | 0.16 | 5.64 | 5.23 | 0.031 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.24 bits, 0.022 ms) → Loom (4.94 bits, 0.006 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.349 |
| Uncompressed | AND | SvS + merge (branchless) | 0.479 |
| Uncompressed | AND | SvS + galloping | 0.011 |
| Uncompressed | AND | SvS + binary search | 0.066 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.016 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.252 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.209 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.008 |
| Uncompressed | AND | Small Adaptive | 0.021 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.049 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.010 |
| Uncompressed | AND | round-robin (LFTJ) | 0.048 |
| Uncompressed | AND | decode + SvS | 0.061 |
| BP128 | AND | leapfrog (NextGEQ) | 0.022 |
| BP128 | AND | round-robin (LFTJ) | 0.083 |
| BP128 | AND | decode + SvS | 0.585 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.018 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.053 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.227 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.024 |
| OptPFor | AND | round-robin (LFTJ) | 0.088 |
| OptPFor | AND | decode + SvS | 0.663 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.019 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.049 |
| Elias-Fano | AND | decode + SvS | 0.868 |
| PEF | AND | leapfrog (NextGEQ) | 0.015 |
| PEF | AND | round-robin (LFTJ) | 0.043 |
| PEF | AND | decode + SvS | 0.749 |
| Loom | AND | native | 0.006 |
| Loom | AND | native, count only | 0.006 |
| Loom | AND | leapfrog (NextGEQ) | 0.014 |
| Loom | AND | round-robin (LFTJ) | 0.040 |
| Loom | AND | decode + SvS | 0.165 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.043 |
| Interpolative | AND | round-robin (LFTJ) | 0.224 |
| Interpolative | AND | decode + SvS | 2.396 |
| Entropic | AND | native | 0.023 |
| Entropic | AND | leapfrog (NextGEQ) | 0.022 |
| Entropic | AND | round-robin (LFTJ) | 0.111 |
| Entropic | AND | decode + SvS | 0.584 |
| Hybrid (bitmap + BP128) | AND | native | 0.022 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.022 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.068 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.754 |
| Roaring | AND | native | 0.010 |
| Roaring | AND | native, count only | 0.011 |
| Roaring | AND | leapfrog (NextGEQ) | 0.012 |
| Roaring | AND | round-robin (LFTJ) | 0.033 |
| Roaring | AND | decode + SvS | 0.411 |
| Pyramid | AND | native | 0.007 |
| Pyramid | AND | native, count only | 0.007 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.013 |
| Pyramid | AND | round-robin (LFTJ) | 0.042 |
| Pyramid | AND | decode + SvS | 0.251 |
| Roaring (roaring crate) | AND | native | 0.044 |
| Roaring (roaring crate) | AND | native, count only | 0.044 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.031 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.063 |
| Roaring (roaring crate) | AND | decode + SvS | 3.246 |
