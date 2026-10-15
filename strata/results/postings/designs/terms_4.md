
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 4.00 terms, 875849 postings) |
| mean AND / OR result | 24 / 836333 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.08 | 0.013 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.61 | 0.0 | 0.03 | 0.88 | 0.74 | 0.029 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.61 | 0.0 | 0.01 | 0.44 | 0.30 | 0.024 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.88 | 0.0 | 0.14 | 1.14 | 0.88 | 0.031 (leapfrog (NextGEQ)) | — |
| Elias-Fano | 10.85 | 7.97 | 0.0 | 0.05 | 1.14 | 1.13 | 0.024 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.86 | 0.0 | 0.19 | 1.12 | 1.00 | 0.019 (leapfrog (NextGEQ)) | — |
| Loom | 6.86 | 4.83 | 0.0 | 0.49 | 0.36 | 0.23 | 0.007 (native) | — |
| Interpolative | 5.97 | 4.85 | 0.0 | 0.09 | 3.23 | 3.13 | 0.059 (leapfrog (NextGEQ)) | — |
| Entropic | 4.97 | 4.18 | 14.3 | 1.85 | 0.95 | 0.78 | 0.030 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.49 | 0.0 | 0.03 | 0.94 | 0.86 | 0.029 (native) | — |
| Roaring | 11.72 | 9.25 | 0.0 | 0.08 | 1.33 | 0.90 | 0.012 (native) | — |
| Pyramid | 10.35 | 6.04 | 0.0 | 0.34 | 0.71 | 0.43 | 0.008 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.25 | 0.0 | 0.16 | 5.64 | 5.22 | 0.036 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.18 bits, 0.030 ms) → Loom (4.83 bits, 0.007 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.449 |
| Uncompressed | AND | SvS + merge (branchless) | 0.617 |
| Uncompressed | AND | SvS + galloping | 0.014 |
| Uncompressed | AND | SvS + binary search | 0.081 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.021 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.318 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.266 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.013 |
| Uncompressed | AND | Small Adaptive | 0.024 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.052 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.014 |
| Uncompressed | AND | round-robin (LFTJ) | 0.050 |
| Uncompressed | AND | decode + SvS | 0.070 |
| BP128 | AND | leapfrog (NextGEQ) | 0.029 |
| BP128 | AND | round-robin (LFTJ) | 0.089 |
| BP128 | AND | decode + SvS | 0.673 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.024 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.060 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.261 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.031 |
| OptPFor | AND | round-robin (LFTJ) | 0.096 |
| OptPFor | AND | decode + SvS | 0.766 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.024 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.052 |
| Elias-Fano | AND | decode + SvS | 0.999 |
| PEF | AND | leapfrog (NextGEQ) | 0.019 |
| PEF | AND | round-robin (LFTJ) | 0.045 |
| PEF | AND | decode + SvS | 0.858 |
| Loom | AND | native | 0.007 |
| Loom | AND | native, count only | 0.007 |
| Loom | AND | leapfrog (NextGEQ) | 0.017 |
| Loom | AND | round-robin (LFTJ) | 0.042 |
| Loom | AND | decode + SvS | 0.190 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.059 |
| Interpolative | AND | round-robin (LFTJ) | 0.241 |
| Interpolative | AND | decode + SvS | 2.765 |
| Entropic | AND | native | 0.031 |
| Entropic | AND | leapfrog (NextGEQ) | 0.030 |
| Entropic | AND | round-robin (LFTJ) | 0.119 |
| Entropic | AND | decode + SvS | 0.669 |
| Hybrid (bitmap + BP128) | AND | native | 0.029 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.030 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.069 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.890 |
| Roaring | AND | native | 0.012 |
| Roaring | AND | native, count only | 0.012 |
| Roaring | AND | leapfrog (NextGEQ) | 0.013 |
| Roaring | AND | round-robin (LFTJ) | 0.030 |
| Roaring | AND | decode + SvS | 0.422 |
| Pyramid | AND | native | 0.008 |
| Pyramid | AND | native, count only | 0.008 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.016 |
| Pyramid | AND | round-robin (LFTJ) | 0.044 |
| Pyramid | AND | decode + SvS | 0.292 |
| Roaring (roaring crate) | AND | native | 0.053 |
| Roaring (roaring crate) | AND | native, count only | 0.052 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.036 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.062 |
| Roaring (roaring crate) | AND | decode + SvS | 3.712 |
