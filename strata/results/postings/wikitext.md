
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

| codec | bits/posting | bits/posting (query lists) | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.21 | 0.48 | 0.25 | 0.022 (SvS + adaptive (SIMD / gallop @64x)) | 0.561 (slices: bitmap) |
| VByte | 11.45 | 8.91 | 0.48 | 3.65 | 1.54 | 0.051 (leapfrog (NextGEQ)) | 0.974 (bitset window (4096)) |
| BP128 | 10.85 | 7.11 | 0.51 | 1.91 | 0.70 | 0.033 (leapfrog (NextGEQ)) | 0.966 (bitset window (4096)) |
| BP128 (bitpacking crate) | 10.85 | 7.11 | 0.45 | 1.59 | 0.45 | 0.030 (leapfrog (NextGEQ)) | 0.855 (bitset window (4096)) |
| OptPFor | 9.95 | 5.91 | 0.73 | 2.04 | 0.87 | 0.037 (leapfrog (NextGEQ)) | 0.898 (bitset window (4096)) |
| Elias-Fano | 8.51 | 5.16 | 0.49 | 2.24 | 1.66 | 0.019 (leapfrog (NextGEQ)) | 0.976 (bitset window (4096)) |
| PEF | 8.92 | 4.75 | 0.91 | 2.56 | 1.61 | 0.022 (leapfrog (NextGEQ)) | 0.931 (bitset window (4096)) |
| Interpolative | 8.46 | 4.77 | 0.77 | 4.66 | 3.94 | 0.081 (leapfrog (NextGEQ)) | 1.623 (bitset window (4096)) |
| Hybrid (bitmap + BP128) | 10.36 | 5.85 | 0.49 | 2.18 | 1.11 | 0.025 (native) | 0.387 (native) |
| Roaring | 15.44 | 8.47 | 0.43 | 1.14 | 0.28 | 0.007 (native) | 0.086 (native) |
| Trie | 23.34 | 6.50 | 4.00 | 15.38 | 3.43 | 0.062 (native) | 0.485 (native) |
| Run-Trie | 23.21 | 6.23 | 3.28 | 19.08 | 4.44 | 0.061 (native) | 0.530 (native) |
| Roaring (roaring crate) | 15.44 | 8.47 | 0.80 | 6.79 | 6.68 | 0.027 (leapfrog (NextGEQ)) | 1.182 (native) |

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.172 |
| Uncompressed | AND | SvS + merge (branchless) | 0.549 |
| Uncompressed | AND | SvS + galloping | 0.024 |
| Uncompressed | AND | SvS + binary search | 0.060 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.032 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.239 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.223 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.171 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.022 |
| Uncompressed | AND | Small Adaptive | 0.024 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.053 |
| Uncompressed | OR | slices: heap | 2.929 |
| Uncompressed | OR | slices: fold (pairwise merge) | 0.859 |
| Uncompressed | OR | slices: bitmap | 0.561 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.029 |
| Uncompressed | AND | round-robin (LFTJ) | 0.059 |
| Uncompressed | AND | decode + SvS | 0.083 |
| Uncompressed | OR | heap | 1.667 |
| Uncompressed | OR | min-scan | 1.168 |
| Uncompressed | OR | bitset window (4096) | 0.602 |
| VByte | AND | leapfrog (NextGEQ) | 0.051 |
| VByte | AND | round-robin (LFTJ) | 0.093 |
| VByte | AND | decode + SvS | 0.268 |
| VByte | OR | heap | 2.008 |
| VByte | OR | min-scan | 1.451 |
| VByte | OR | bitset window (4096) | 0.974 |
| BP128 | AND | leapfrog (NextGEQ) | 0.033 |
| BP128 | AND | round-robin (LFTJ) | 0.056 |
| BP128 | AND | decode + SvS | 0.164 |
| BP128 | OR | heap | 1.839 |
| BP128 | OR | min-scan | 1.167 |
| BP128 | OR | bitset window (4096) | 0.966 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.030 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.055 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.113 |
| BP128 (bitpacking crate) | OR | heap | 1.811 |
| BP128 (bitpacking crate) | OR | min-scan | 1.198 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 0.855 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.037 |
| OptPFor | AND | round-robin (LFTJ) | 0.066 |
| OptPFor | AND | decode + SvS | 0.193 |
| OptPFor | OR | heap | 1.971 |
| OptPFor | OR | min-scan | 1.186 |
| OptPFor | OR | bitset window (4096) | 0.898 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.019 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.032 |
| Elias-Fano | AND | decode + SvS | 0.394 |
| Elias-Fano | OR | heap | 2.009 |
| Elias-Fano | OR | min-scan | 1.300 |
| Elias-Fano | OR | bitset window (4096) | 0.976 |
| PEF | AND | leapfrog (NextGEQ) | 0.022 |
| PEF | AND | round-robin (LFTJ) | 0.034 |
| PEF | AND | decode + SvS | 0.300 |
| PEF | OR | heap | 2.178 |
| PEF | OR | min-scan | 1.396 |
| PEF | OR | bitset window (4096) | 0.931 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.081 |
| Interpolative | AND | round-robin (LFTJ) | 0.210 |
| Interpolative | AND | decode + SvS | 0.932 |
| Interpolative | OR | heap | 2.718 |
| Interpolative | OR | min-scan | 1.953 |
| Interpolative | OR | bitset window (4096) | 1.623 |
| Hybrid (bitmap + BP128) | AND | native | 0.025 |
| Hybrid (bitmap + BP128) | OR | native | 0.387 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.026 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.040 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.241 |
| Hybrid (bitmap + BP128) | OR | heap | 2.445 |
| Hybrid (bitmap + BP128) | OR | min-scan | 1.822 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 1.903 |
| Roaring | AND | native | 0.007 |
| Roaring | OR | native | 0.086 |
| Roaring | AND | native, count only | 0.008 |
| Roaring | OR | native, count only | 0.046 |
| Roaring | AND | leapfrog (NextGEQ) | 0.016 |
| Roaring | AND | round-robin (LFTJ) | 0.024 |
| Roaring | AND | decode + SvS | 0.066 |
| Roaring | OR | heap | 1.906 |
| Roaring | OR | min-scan | 1.353 |
| Roaring | OR | bitset window (4096) | 0.774 |
| Trie | AND | native | 0.062 |
| Trie | OR | native | 0.485 |
| Trie | AND | leapfrog (NextGEQ) | 0.084 |
| Trie | AND | round-robin (LFTJ) | 0.117 |
| Trie | AND | decode + SvS | 0.297 |
| Trie | OR | heap | 2.413 |
| Trie | OR | min-scan | 1.617 |
| Trie | OR | bitset window (4096) | 1.407 |
| Run-Trie | AND | native | 0.061 |
| Run-Trie | OR | native | 0.530 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.080 |
| Run-Trie | AND | round-robin (LFTJ) | 0.120 |
| Run-Trie | AND | decode + SvS | 0.330 |
| Run-Trie | OR | heap | 2.515 |
| Run-Trie | OR | min-scan | 1.675 |
| Run-Trie | OR | bitset window (4096) | 1.507 |
| Roaring (roaring crate) | AND | native | 0.049 |
| Roaring (roaring crate) | OR | native | 1.182 |
| Roaring (roaring crate) | AND | native, count only | 0.046 |
| Roaring (roaring crate) | OR | native, count only | 0.083 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.027 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.040 |
| Roaring (roaring crate) | AND | decode + SvS | 1.122 |
| Roaring (roaring crate) | OR | heap | 2.354 |
| Roaring (roaring crate) | OR | min-scan | 1.952 |
| Roaring (roaring crate) | OR | bitset window (4096) | 1.464 |
