
## Collection and workload

| | |
|---|---|
| collection | wikitext103.txt |
| documents | 859532 |
| lists / postings | 531033 / 57010525 |
| queries | 500 (mean 3.02 terms, 221776 postings) |
| mean AND / OR result | 64 / 199635 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.22 | 0.50 | 0.28 | 0.022 (SvS + adaptive (SIMD / gallop @64x)) | 0.555 (slices: bitmap) |
| VByte | 11.45 | 8.91 | 0.0 | 0.52 | 3.66 | 1.55 | 0.050 (leapfrog (NextGEQ)) | 0.945 (bitset window (4096)) |
| BP128 | 10.85 | 7.11 | 0.0 | 0.47 | 1.82 | 0.67 | 0.032 (leapfrog (NextGEQ)) | 0.772 (bitset window (4096)) |
| BP128 (bitpacking crate) | 10.85 | 7.11 | 0.0 | 0.39 | 1.54 | 0.43 | 0.026 (leapfrog (NextGEQ)) | 0.761 (bitset window (4096)) |
| OptPFor | 9.95 | 5.91 | 0.0 | 0.67 | 2.04 | 0.87 | 0.032 (leapfrog (NextGEQ)) | 0.826 (bitset window (4096)) |
| Elias-Fano | 8.51 | 5.16 | 0.0 | 0.40 | 2.37 | 1.85 | 0.019 (leapfrog (NextGEQ)) | 0.793 (bitset window (4096)) |
| PEF | 8.92 | 4.75 | 0.0 | 0.77 | 2.62 | 1.39 | 0.018 (leapfrog (NextGEQ)) | 0.920 (bitset window (4096)) |
| Loom | 8.96 | 4.74 | 0.0 | 1.52 | 1.22 | 0.31 | 0.010 (native) | 0.104 (native) |
| Interpolative | 8.46 | 4.77 | 0.0 | 0.78 | 4.94 | 3.94 | 0.087 (leapfrog (NextGEQ)) | 1.486 (bitset window (4096)) |
| Entropic | 7.25 | 4.18 | 22.4 | 9.89 | 3.13 | 1.39 | 0.039 (native) | 0.424 (native) |
| Hybrid (bitmap + BP128) | 10.36 | 5.85 | 0.0 | 0.50 | 2.00 | 1.00 | 0.025 (native) | 0.391 (native) |
| Roaring | 15.44 | 8.47 | 0.0 | 0.44 | 1.14 | 0.26 | 0.007 (native) | 0.083 (native) |
| Pyramid | 12.05 | 6.30 | 0.0 | 1.43 | 1.56 | 0.33 | 0.010 (native) | 0.068 (native) |
| Trie | 23.34 | 6.50 | 0.0 | 3.76 | 15.55 | 3.18 | 0.054 (native) | 0.472 (native) |
| Run-Trie | 23.21 | 6.23 | 0.0 | 3.28 | 17.33 | 4.14 | 0.058 (native) | 0.505 (native) |
| Roaring (roaring crate) | 15.44 | 8.47 | 0.0 | 0.71 | 6.46 | 5.29 | 0.027 (leapfrog (NextGEQ)) | 1.139 (native) |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: Entropic (4.18 bits, 0.039 ms) → Loom (4.74 bits, 0.010 ms) → Roaring (8.47 bits, 0.007 ms)
* OR: Entropic (4.18 bits, 0.424 ms) → Loom (4.74 bits, 0.104 ms) → Pyramid (6.30 bits, 0.068 ms)

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.148 |
| Uncompressed | AND | SvS + merge (branchless) | 0.579 |
| Uncompressed | AND | SvS + galloping | 0.023 |
| Uncompressed | AND | SvS + binary search | 0.057 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.036 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.235 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.232 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.174 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.022 |
| Uncompressed | AND | Small Adaptive | 0.025 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.065 |
| Uncompressed | OR | slices: heap | 2.883 |
| Uncompressed | OR | slices: fold (pairwise merge) | 0.823 |
| Uncompressed | OR | slices: bitmap | 0.555 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.029 |
| Uncompressed | AND | round-robin (LFTJ) | 0.056 |
| Uncompressed | AND | decode + SvS | 0.078 |
| Uncompressed | OR | heap | 1.660 |
| Uncompressed | OR | min-scan | 1.158 |
| Uncompressed | OR | bitset window (4096) | 0.622 |
| VByte | AND | leapfrog (NextGEQ) | 0.050 |
| VByte | AND | round-robin (LFTJ) | 0.087 |
| VByte | AND | decode + SvS | 0.294 |
| VByte | OR | heap | 2.017 |
| VByte | OR | min-scan | 1.284 |
| VByte | OR | bitset window (4096) | 0.945 |
| BP128 | AND | leapfrog (NextGEQ) | 0.032 |
| BP128 | AND | round-robin (LFTJ) | 0.057 |
| BP128 | AND | decode + SvS | 0.151 |
| BP128 | OR | heap | 1.882 |
| BP128 | OR | min-scan | 1.211 |
| BP128 | OR | bitset window (4096) | 0.772 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.026 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.047 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.111 |
| BP128 (bitpacking crate) | OR | heap | 1.807 |
| BP128 (bitpacking crate) | OR | min-scan | 1.221 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 0.761 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.032 |
| OptPFor | AND | round-robin (LFTJ) | 0.055 |
| OptPFor | AND | decode + SvS | 0.182 |
| OptPFor | OR | heap | 1.874 |
| OptPFor | OR | min-scan | 1.284 |
| OptPFor | OR | bitset window (4096) | 0.826 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.019 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.031 |
| Elias-Fano | AND | decode + SvS | 0.350 |
| Elias-Fano | OR | heap | 2.119 |
| Elias-Fano | OR | min-scan | 1.133 |
| Elias-Fano | OR | bitset window (4096) | 0.793 |
| PEF | AND | leapfrog (NextGEQ) | 0.018 |
| PEF | AND | round-robin (LFTJ) | 0.031 |
| PEF | AND | decode + SvS | 0.256 |
| PEF | OR | heap | 2.042 |
| PEF | OR | min-scan | 1.373 |
| PEF | OR | bitset window (4096) | 0.920 |
| Loom | AND | native | 0.010 |
| Loom | OR | native | 0.104 |
| Loom | AND | native, count only | 0.009 |
| Loom | OR | native, count only | 0.054 |
| Loom | AND | leapfrog (NextGEQ) | 0.019 |
| Loom | AND | round-robin (LFTJ) | 0.030 |
| Loom | AND | decode + SvS | 0.071 |
| Loom | OR | heap | 1.862 |
| Loom | OR | min-scan | 1.276 |
| Loom | OR | bitset window (4096) | 0.801 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.087 |
| Interpolative | AND | round-robin (LFTJ) | 0.175 |
| Interpolative | AND | decode + SvS | 0.907 |
| Interpolative | OR | heap | 2.522 |
| Interpolative | OR | min-scan | 1.924 |
| Interpolative | OR | bitset window (4096) | 1.486 |
| Entropic | AND | native | 0.039 |
| Entropic | OR | native | 0.424 |
| Entropic | AND | leapfrog (NextGEQ) | 0.041 |
| Entropic | AND | round-robin (LFTJ) | 0.096 |
| Entropic | AND | decode + SvS | 0.288 |
| Entropic | OR | heap | 2.265 |
| Entropic | OR | min-scan | 1.569 |
| Entropic | OR | bitset window (4096) | 1.300 |
| Hybrid (bitmap + BP128) | AND | native | 0.025 |
| Hybrid (bitmap + BP128) | OR | native | 0.391 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.029 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.037 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.240 |
| Hybrid (bitmap + BP128) | OR | heap | 2.166 |
| Hybrid (bitmap + BP128) | OR | min-scan | 1.792 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 1.754 |
| Roaring | AND | native | 0.007 |
| Roaring | OR | native | 0.083 |
| Roaring | AND | native, count only | 0.007 |
| Roaring | OR | native, count only | 0.044 |
| Roaring | AND | leapfrog (NextGEQ) | 0.015 |
| Roaring | AND | round-robin (LFTJ) | 0.024 |
| Roaring | AND | decode + SvS | 0.067 |
| Roaring | OR | heap | 1.849 |
| Roaring | OR | min-scan | 1.280 |
| Roaring | OR | bitset window (4096) | 0.706 |
| Pyramid | AND | native | 0.010 |
| Pyramid | OR | native | 0.068 |
| Pyramid | AND | native, count only | 0.010 |
| Pyramid | OR | native, count only | 0.021 |
| Pyramid | AND | leapfrog (NextGEQ) | 0.018 |
| Pyramid | AND | round-robin (LFTJ) | 0.027 |
| Pyramid | AND | decode + SvS | 0.070 |
| Pyramid | OR | heap | 2.242 |
| Pyramid | OR | min-scan | 1.685 |
| Pyramid | OR | bitset window (4096) | 1.597 |
| Trie | AND | native | 0.054 |
| Trie | OR | native | 0.472 |
| Trie | AND | leapfrog (NextGEQ) | 0.082 |
| Trie | AND | round-robin (LFTJ) | 0.112 |
| Trie | AND | decode + SvS | 0.270 |
| Trie | OR | heap | 2.291 |
| Trie | OR | min-scan | 1.676 |
| Trie | OR | bitset window (4096) | 1.344 |
| Run-Trie | AND | native | 0.058 |
| Run-Trie | OR | native | 0.505 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.080 |
| Run-Trie | AND | round-robin (LFTJ) | 0.115 |
| Run-Trie | AND | decode + SvS | 0.342 |
| Run-Trie | OR | heap | 2.369 |
| Run-Trie | OR | min-scan | 1.772 |
| Run-Trie | OR | bitset window (4096) | 1.396 |
| Roaring (roaring crate) | AND | native | 0.049 |
| Roaring (roaring crate) | OR | native | 1.139 |
| Roaring (roaring crate) | AND | native, count only | 0.047 |
| Roaring (roaring crate) | OR | native, count only | 0.078 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.027 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.038 |
| Roaring (roaring crate) | AND | decode + SvS | 1.208 |
| Roaring (roaring crate) | OR | heap | 2.286 |
| Roaring (roaring crate) | OR | min-scan | 1.959 |
| Roaring (roaring crate) | OR | bitset window (4096) | 1.503 |
