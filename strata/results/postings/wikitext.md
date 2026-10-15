
## Collection and workload

| | |
|---|---|
| collection | wikitext103.txt |
| documents | 859532 |
| lists / postings | 531033 / 57010525 |
| queries | 500 (mean 3.02 terms, 221776 postings) |
| mean AND / OR result | 64 / 199635 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.04 | 0.17 | 0.08 | 0.009 (SvS + adaptive (SIMD / gallop @64x)) | 0.496 (slices: fold (pairwise merge)) |
| VByte | 11.45 | 8.91 | 0.0 | 0.19 | 2.31 | 1.24 | 0.037 (leapfrog (NextGEQ)) | 0.885 (bitset window (4096)) |
| BP128 | 10.85 | 7.11 | 0.0 | 0.16 | 1.14 | 0.78 | 0.027 (leapfrog (NextGEQ)) | 0.848 (bitset window (4096)) |
| BP128 (bitpacking crate) | 10.85 | 7.11 | 0.0 | 0.11 | 0.70 | 0.31 | 0.020 (leapfrog (NextGEQ)) | 0.754 (bitset window (4096)) |
| OptPFor | 9.95 | 5.91 | 0.0 | 0.36 | 1.23 | 0.84 | 0.028 (leapfrog (NextGEQ)) | 0.856 (bitset window (4096)) |
| Elias-Fano | 8.51 | 5.16 | 0.0 | 0.21 | 1.24 | 1.11 | 0.018 (leapfrog (NextGEQ)) | 0.834 (bitset window (4096)) |
| PEF | 8.92 | 4.75 | 0.0 | 0.66 | 1.30 | 1.04 | 0.016 (leapfrog (NextGEQ)) | 0.826 (bitset window (4096)) |
| Loom | 8.96 | 4.74 | 0.0 | 1.44 | 0.48 | 0.27 | 0.007 (native) | 0.107 (native) |
| Interpolative | 8.46 | 4.77 | 0.0 | 0.34 | 3.03 | 2.99 | 0.060 (leapfrog (NextGEQ)) | 1.313 (bitset window (4096)) |
| Entropic | 7.25 | 4.18 | 22.4 | 5.73 | 1.32 | 0.80 | 0.028 (native) | 0.279 (native) |
| Hybrid (bitmap + BP128) | 10.36 | 5.85 | 0.0 | 0.17 | 1.19 | 0.87 | 0.019 (native) | 0.305 (native) |
| Roaring | 15.44 | 8.47 | 0.0 | 0.17 | 0.45 | 0.29 | 0.004 (native) | 0.069 (native) |
| Pyramid | 12.05 | 6.30 | 0.0 | 0.99 | 0.92 | 0.45 | 0.008 (native) | 0.080 (native) |
| Trie | 23.34 | 6.50 | 0.0 | 1.83 | 17.83 | 7.85 | 0.048 (native) | 0.821 (native) |
| Run-Trie | 23.21 | 6.23 | 0.0 | 1.87 | 19.20 | 8.38 | 0.049 (native) | 0.866 (native) |
| Roaring (roaring crate) | 15.44 | 8.47 | 0.0 | 0.36 | 4.50 | 4.53 | 0.018 (leapfrog (NextGEQ)) | 0.805 (bitset window (4096)) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.18 bits, 0.028 ms) → Loom (4.74 bits, 0.007 ms) → Roaring (8.47 bits, 0.004 ms)
* OR: Entropic (4.18 bits, 0.279 ms) → Loom (4.74 bits, 0.107 ms) → Pyramid (6.30 bits, 0.080 ms) → Roaring (8.47 bits, 0.069 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.158 |
| Uncompressed | AND | SvS + merge (branchless) | 0.213 |
| Uncompressed | AND | SvS + galloping | 0.014 |
| Uncompressed | AND | SvS + binary search | 0.038 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.017 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.123 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.094 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.009 |
| Uncompressed | AND | Small Adaptive | 0.014 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.028 |
| Uncompressed | OR | slices: heap | 1.620 |
| Uncompressed | OR | slices: fold (pairwise merge) | 0.496 |
| Uncompressed | OR | slices: bitmap | 0.659 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.019 |
| Uncompressed | AND | round-robin (LFTJ) | 0.033 |
| Uncompressed | AND | decode + SvS | 0.023 |
| Uncompressed | OR | heap | 1.051 |
| Uncompressed | OR | min-scan | 0.718 |
| Uncompressed | OR | bitset window (4096) | 0.601 |
| VByte | AND | leapfrog (NextGEQ) | 0.037 |
| VByte | AND | round-robin (LFTJ) | 0.068 |
| VByte | AND | decode + SvS | 0.235 |
| VByte | OR | heap | 1.346 |
| VByte | OR | min-scan | 0.900 |
| VByte | OR | bitset window (4096) | 0.885 |
| BP128 | AND | leapfrog (NextGEQ) | 0.027 |
| BP128 | AND | round-robin (LFTJ) | 0.050 |
| BP128 | AND | decode + SvS | 0.179 |
| BP128 | OR | heap | 1.316 |
| BP128 | OR | min-scan | 0.858 |
| BP128 | OR | bitset window (4096) | 0.848 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.020 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.033 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.075 |
| BP128 (bitpacking crate) | OR | heap | 1.197 |
| BP128 (bitpacking crate) | OR | min-scan | 0.757 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 0.754 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.028 |
| OptPFor | AND | round-robin (LFTJ) | 0.052 |
| OptPFor | AND | decode + SvS | 0.188 |
| OptPFor | OR | heap | 1.314 |
| OptPFor | OR | min-scan | 0.878 |
| OptPFor | OR | bitset window (4096) | 0.856 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.018 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.027 |
| Elias-Fano | AND | decode + SvS | 0.262 |
| Elias-Fano | OR | heap | 1.419 |
| Elias-Fano | OR | min-scan | 1.059 |
| Elias-Fano | OR | bitset window (4096) | 0.834 |
| PEF | AND | leapfrog (NextGEQ) | 0.016 |
| PEF | AND | round-robin (LFTJ) | 0.024 |
| PEF | AND | decode + SvS | 0.210 |
| PEF | OR | heap | 1.453 |
| PEF | OR | min-scan | 0.903 |
| PEF | OR | bitset window (4096) | 0.826 |
| Loom | AND | native | 0.007 |
| Loom | OR | native | 0.107 |
| Loom | AND | native, count only | 0.007 |
| Loom | OR | native, count only | 0.064 |
| Loom | AND | leapfrog (NextGEQ) | 0.016 |
| Loom | AND | round-robin (LFTJ) | 0.022 |
| Loom | AND | decode + SvS | 0.043 |
| Loom | OR | heap | 1.263 |
| Loom | OR | min-scan | 0.901 |
| Loom | OR | bitset window (4096) | 0.767 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.060 |
| Interpolative | AND | round-robin (LFTJ) | 0.132 |
| Interpolative | AND | decode + SvS | 0.653 |
| Interpolative | OR | heap | 1.786 |
| Interpolative | OR | min-scan | 1.329 |
| Interpolative | OR | bitset window (4096) | 1.313 |
| Entropic | AND | native | 0.028 |
| Entropic | OR | native | 0.279 |
| Entropic | AND | leapfrog (NextGEQ) | 0.029 |
| Entropic | AND | round-robin (LFTJ) | 0.063 |
| Entropic | AND | decode + SvS | 0.175 |
| Entropic | OR | heap | 1.834 |
| Entropic | OR | min-scan | 1.507 |
| Entropic | OR | bitset window (4096) | 1.434 |
| Hybrid (bitmap + BP128) | AND | native | 0.019 |
| Hybrid (bitmap + BP128) | OR | native | 0.305 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.020 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.029 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.203 |
| Hybrid (bitmap + BP128) | OR | heap | 1.744 |
| Hybrid (bitmap + BP128) | OR | min-scan | 1.295 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 1.157 |
| Roaring | AND | native | 0.004 |
| Roaring | OR | native | 0.069 |
| Roaring | AND | native, count only | 0.004 |
| Roaring | OR | native, count only | 0.038 |
| Roaring | AND | leapfrog (NextGEQ) | 0.009 |
| Roaring | AND | round-robin (LFTJ) | 0.013 |
| Roaring | AND | decode + SvS | 0.048 |
| Roaring | OR | heap | 1.261 |
| Roaring | OR | min-scan | 0.881 |
| Roaring | OR | bitset window (4096) | 0.813 |
| Pyramid | AND | native | 0.008 |
| Pyramid | OR | native | 0.080 |
| Pyramid | AND | native, count only | 0.008 |
| Pyramid | OR | native, count only | 0.029 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.020 |
| Pyramid | AND | round-robin (LFTJ) | 0.028 |
| Pyramid | AND | decode + SvS | 0.060 |
| Pyramid | OR | heap | 1.861 |
| Pyramid | OR | min-scan | 1.399 |
| Pyramid | OR | bitset window (4096) | 1.531 |
| Trie | AND | native | 0.048 |
| Trie | OR | native | 0.821 |
| Trie | AND | leapfrog (NextGEQ) | 0.077 |
| Trie | AND | round-robin (LFTJ) | 0.113 |
| Trie | AND | decode + SvS | 0.692 |
| Trie | OR | heap | 2.266 |
| Trie | OR | min-scan | 1.749 |
| Trie | OR | bitset window (4096) | 1.553 |
| Run-Trie | AND | native | 0.049 |
| Run-Trie | OR | native | 0.866 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.081 |
| Run-Trie | AND | round-robin (LFTJ) | 0.118 |
| Run-Trie | AND | decode + SvS | 0.743 |
| Run-Trie | OR | heap | 2.312 |
| Run-Trie | OR | min-scan | 1.851 |
| Run-Trie | OR | bitset window (4096) | 1.596 |
| Roaring (roaring crate) | AND | native | 0.028 |
| Roaring (roaring crate) | OR | native | 0.838 |
| Roaring (roaring crate) | AND | native, count only | 0.028 |
| Roaring (roaring crate) | OR | native, count only | 0.067 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.018 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.023 |
| Roaring (roaring crate) | AND | decode + SvS | 0.862 |
| Roaring (roaring crate) | OR | heap | 1.641 |
| Roaring (roaring crate) | OR | min-scan | 1.479 |
| Roaring (roaring crate) | OR | bitset window (4096) | 0.805 |
