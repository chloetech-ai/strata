
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 6.00 terms, 735851 postings) |
| mean AND / OR result | 2 / 686811 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.08 | 0.005 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.80 | 0.0 | 0.03 | 0.88 | 0.77 | 0.016 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.80 | 0.0 | 0.01 | 0.44 | 0.30 | 0.013 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.98 | 0.0 | 0.14 | 1.14 | 0.90 | 0.017 (leapfrog (NextGEQ)) | — |
| Elias-Fano | 10.85 | 8.32 | 0.0 | 0.05 | 1.14 | 1.14 | 0.014 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 5.03 | 0.0 | 0.18 | 1.12 | 1.01 | 0.011 (leapfrog (NextGEQ)) | — |
| Loom | 6.86 | 4.99 | 0.0 | 0.49 | 0.35 | 0.24 | 0.004 (native) | — |
| Interpolative | 5.97 | 4.96 | 0.0 | 0.09 | 3.22 | 3.14 | 0.032 (leapfrog (NextGEQ)) | — |
| Entropic | 4.97 | 4.27 | 14.3 | 1.87 | 0.95 | 0.79 | 0.016 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.69 | 0.0 | 0.03 | 0.94 | 0.86 | 0.016 (native) | — |
| Roaring | 11.72 | 9.43 | 0.0 | 0.08 | 1.38 | 1.00 | 0.007 (native) | — |
| Pyramid | 10.35 | 6.40 | 0.0 | 0.34 | 0.71 | 0.46 | 0.006 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.43 | 0.0 | 0.16 | 5.64 | 5.26 | 0.026 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.27 bits, 0.016 ms) → Loom (4.99 bits, 0.004 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.313 |
| Uncompressed | AND | SvS + merge (branchless) | 0.426 |
| Uncompressed | AND | SvS + galloping | 0.008 |
| Uncompressed | AND | SvS + binary search | 0.048 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.011 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.235 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.196 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.005 |
| Uncompressed | AND | Small Adaptive | 0.017 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.045 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.007 |
| Uncompressed | AND | round-robin (LFTJ) | 0.044 |
| Uncompressed | AND | decode + SvS | 0.059 |
| BP128 | AND | leapfrog (NextGEQ) | 0.016 |
| BP128 | AND | round-robin (LFTJ) | 0.072 |
| BP128 | AND | decode + SvS | 0.562 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.013 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.046 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.217 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.017 |
| OptPFor | AND | round-robin (LFTJ) | 0.076 |
| OptPFor | AND | decode + SvS | 0.640 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.014 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.042 |
| Elias-Fano | AND | decode + SvS | 0.837 |
| PEF | AND | leapfrog (NextGEQ) | 0.011 |
| PEF | AND | round-robin (LFTJ) | 0.037 |
| PEF | AND | decode + SvS | 0.720 |
| Loom | AND | native | 0.004 |
| Loom | AND | native, count only | 0.004 |
| Loom | AND | leapfrog (NextGEQ) | 0.010 |
| Loom | AND | round-robin (LFTJ) | 0.034 |
| Loom | AND | decode + SvS | 0.158 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.032 |
| Interpolative | AND | round-robin (LFTJ) | 0.194 |
| Interpolative | AND | decode + SvS | 2.311 |
| Entropic | AND | native | 0.017 |
| Entropic | AND | leapfrog (NextGEQ) | 0.016 |
| Entropic | AND | round-robin (LFTJ) | 0.096 |
| Entropic | AND | decode + SvS | 0.561 |
| Hybrid (bitmap + BP128) | AND | native | 0.016 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.016 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.060 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.712 |
| Roaring | AND | native | 0.007 |
| Roaring | AND | native, count only | 0.007 |
| Roaring | AND | leapfrog (NextGEQ) | 0.008 |
| Roaring | AND | round-robin (LFTJ) | 0.026 |
| Roaring | AND | decode + SvS | 0.411 |
| Pyramid | AND | native | 0.006 |
| Pyramid | AND | native, count only | 0.006 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.009 |
| Pyramid | AND | round-robin (LFTJ) | 0.034 |
| Pyramid | AND | decode + SvS | 0.243 |
| Roaring (roaring crate) | AND | native | 0.034 |
| Roaring (roaring crate) | AND | native, count only | 0.034 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.026 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.059 |
| Roaring (roaring crate) | AND | decode + SvS | 3.149 |
