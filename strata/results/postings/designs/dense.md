
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 2.64 terms, 1787965 postings) |
| mean AND / OR result | 42798 / 1628738 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.05 | 0.23 | 0.26 | 1.578 (SvS + adaptive (SIMD / gallop @64x)) | 4.414 (slices: bitmap) |
| BP128 | 10.07 | 6.38 | 0.0 | 0.06 | 0.85 | 0.69 | 2.708 (decode + SvS) | 6.677 (bitset window (4096)) |
| BP128 (bitpacking crate) | 10.07 | 6.38 | 0.0 | 0.04 | 0.90 | 0.43 | 2.300 (decode + SvS) | 6.256 (bitset window (4096)) |
| OptPFor | 7.13 | 5.09 | 0.0 | 0.21 | 1.14 | 0.67 | 2.834 (decode + SvS) | 7.050 (bitset window (4096)) |
| Elias-Fano | 10.85 | 5.44 | 0.0 | 0.05 | 1.61 | 1.58 | 3.739 (leapfrog (NextGEQ)) | 7.192 (bitset window (4096)) |
| PEF | 7.04 | 3.71 | 0.0 | 0.18 | 1.56 | 1.14 | 2.684 (leapfrog (NextGEQ)) | 7.892 (bitset window (4096)) |
| Loom | 6.86 | 3.69 | 0.0 | 0.48 | 0.46 | 0.27 | 0.803 (native) | 1.238 (native) |
| Interpolative | 5.97 | 3.93 | 0.0 | 0.14 | 4.21 | 3.94 | 9.447 (decode + SvS) | 13.656 (bitset window (4096)) |
| Entropic | 4.97 | 3.40 | 14.3 | 2.68 | 1.64 | 1.31 | 4.150 (decode + SvS) | 3.910 (native) |
| Hybrid (bitmap + BP128) | 10.01 | 6.14 | 0.0 | 0.06 | 0.99 | 1.08 | 3.392 (native) | 6.568 (native) |
| Roaring | 11.72 | 8.03 | 0.0 | 0.13 | 2.67 | 0.84 | 0.776 (native) | 1.019 (native) |
| Pyramid | 10.35 | 4.62 | 0.0 | 0.27 | 0.99 | 0.25 | 0.766 (native) | 0.674 (native) |
| Roaring (roaring crate) | 11.72 | 8.03 | 0.0 | 0.25 | 7.53 | 5.97 | 1.588 (native) | 11.640 (native) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (3.40 bits, 4.150 ms) → Loom (3.69 bits, 0.803 ms) → Pyramid (4.62 bits, 0.766 ms)
* OR: Entropic (3.40 bits, 3.910 ms) → Loom (3.69 bits, 1.238 ms) → Pyramid (4.62 bits, 0.674 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 3.190 |
| Uncompressed | AND | SvS + merge (branchless) | 5.747 |
| Uncompressed | AND | SvS + galloping | 2.868 |
| Uncompressed | AND | SvS + binary search | 19.412 |
| Uncompressed | AND | SvS + Baeza-Yates | 4.174 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 2.294 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 2.440 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 1.936 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 1.578 |
| Uncompressed | AND | Small Adaptive | 4.286 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 4.498 |
| Uncompressed | OR | slices: heap | 23.125 |
| Uncompressed | OR | slices: fold (pairwise merge) | 7.404 |
| Uncompressed | OR | slices: bitmap | 4.414 |
| Uncompressed | AND | leapfrog (NextGEQ) | 3.052 |
| Uncompressed | AND | round-robin (LFTJ) | 3.845 |
| Uncompressed | AND | decode + SvS | 2.172 |
| Uncompressed | OR | heap | 15.223 |
| Uncompressed | OR | min-scan | 10.851 |
| Uncompressed | OR | bitset window (4096) | 4.778 |
| BP128 | AND | leapfrog (NextGEQ) | 5.086 |
| BP128 | AND | round-robin (LFTJ) | 5.696 |
| BP128 | AND | decode + SvS | 2.708 |
| BP128 | OR | heap | 17.480 |
| BP128 | OR | min-scan | 12.016 |
| BP128 | OR | bitset window (4096) | 6.677 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 4.885 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 5.468 |
| BP128 (bitpacking crate) | AND | decode + SvS | 2.300 |
| BP128 (bitpacking crate) | OR | heap | 17.577 |
| BP128 (bitpacking crate) | OR | min-scan | 11.274 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 6.256 |
| OptPFor | AND | leapfrog (NextGEQ) | 5.337 |
| OptPFor | AND | round-robin (LFTJ) | 5.797 |
| OptPFor | AND | decode + SvS | 2.834 |
| OptPFor | OR | heap | 18.114 |
| OptPFor | OR | min-scan | 11.953 |
| OptPFor | OR | bitset window (4096) | 7.050 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 3.739 |
| Elias-Fano | AND | round-robin (LFTJ) | 4.765 |
| Elias-Fano | AND | decode + SvS | 4.455 |
| Elias-Fano | OR | heap | 18.265 |
| Elias-Fano | OR | min-scan | 11.356 |
| Elias-Fano | OR | bitset window (4096) | 7.192 |
| PEF | AND | leapfrog (NextGEQ) | 2.684 |
| PEF | AND | round-robin (LFTJ) | 3.400 |
| PEF | AND | decode + SvS | 3.990 |
| PEF | OR | heap | 19.846 |
| PEF | OR | min-scan | 13.645 |
| PEF | OR | bitset window (4096) | 7.892 |
| Loom | AND | native | 0.803 |
| Loom | OR | native | 1.238 |
| Loom | AND | native, count only | 0.758 |
| Loom | OR | native, count only | 0.501 |
| Loom | AND | leapfrog (NextGEQ) | 2.860 |
| Loom | AND | round-robin (LFTJ) | 3.291 |
| Loom | AND | decode + SvS | 2.293 |
| Loom | OR | heap | 18.024 |
| Loom | OR | min-scan | 12.865 |
| Loom | OR | bitset window (4096) | 6.773 |
| Interpolative | AND | leapfrog (NextGEQ) | 9.806 |
| Interpolative | AND | round-robin (LFTJ) | 11.765 |
| Interpolative | AND | decode + SvS | 9.447 |
| Interpolative | OR | heap | 25.070 |
| Interpolative | OR | min-scan | 17.237 |
| Interpolative | OR | bitset window (4096) | 13.656 |
| Entropic | AND | native | 4.390 |
| Entropic | OR | native | 3.910 |
| Entropic | AND | leapfrog (NextGEQ) | 5.246 |
| Entropic | AND | round-robin (LFTJ) | 6.749 |
| Entropic | AND | decode + SvS | 4.150 |
| Entropic | OR | heap | 23.014 |
| Entropic | OR | min-scan | 15.289 |
| Entropic | OR | bitset window (4096) | 11.674 |
| Hybrid (bitmap + BP128) | AND | native | 3.392 |
| Hybrid (bitmap + BP128) | OR | native | 6.568 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 4.268 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 4.212 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 3.646 |
| Hybrid (bitmap + BP128) | OR | heap | 22.066 |
| Hybrid (bitmap + BP128) | OR | min-scan | 17.027 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 13.104 |
| Roaring | AND | native | 0.776 |
| Roaring | OR | native | 1.019 |
| Roaring | AND | native, count only | 0.716 |
| Roaring | OR | native, count only | 0.587 |
| Roaring | AND | leapfrog (NextGEQ) | 2.713 |
| Roaring | AND | round-robin (LFTJ) | 2.755 |
| Roaring | AND | decode + SvS | 2.772 |
| Roaring | OR | heap | 20.555 |
| Roaring | OR | min-scan | 13.331 |
| Roaring | OR | bitset window (4096) | 7.779 |
| Pyramid | AND | native | 0.766 |
| Pyramid | OR | native | 0.674 |
| Pyramid | AND | native, count only | 0.691 |
| Pyramid | OR | native, count only | 0.230 |
| Pyramid | AND | leapfrog (NextGEQ) | 2.854 |
| Pyramid | AND | round-robin (LFTJ) | 3.355 |
| Pyramid | AND | decode + SvS | 2.188 |
| Pyramid | OR | heap | 23.187 |
| Pyramid | OR | min-scan | 18.460 |
| Pyramid | OR | bitset window (4096) | 14.120 |
| Roaring (roaring crate) | AND | native | 1.588 |
| Roaring (roaring crate) | OR | native | 11.640 |
| Roaring (roaring crate) | AND | native, count only | 1.467 |
| Roaring (roaring crate) | OR | native, count only | 2.879 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 5.232 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 5.885 |
| Roaring (roaring crate) | AND | decode + SvS | 12.638 |
| Roaring (roaring crate) | OR | heap | 24.453 |
| Roaring (roaring crate) | OR | min-scan | 18.893 |
| Roaring (roaring crate) | OR | bitset window (4096) | 13.707 |
