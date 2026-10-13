
## Collection and workload

| | |
|---|---|
| collection | synthetic-uniform (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 500 (mean 2.70 terms, 686098 postings) |
| mean AND / OR result | 119 / 675834 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.05 | 0.23 | 0.23 | 0.100 (SvS + adaptive (SIMD / gallop @64x)) | 2.051 (slices: bitmap) |
| VByte | 13.92 | 10.91 | 0.09 | 3.02 | 2.62 | 0.271 (leapfrog (NextGEQ)) | 3.633 (bitset window (4096)) |
| BP128 | 12.38 | 9.39 | 0.05 | 0.91 | 0.63 | 0.156 (leapfrog (NextGEQ)) | 3.403 (bitset window (4096)) |
| BP128 (bitpacking crate) | 12.38 | 9.39 | 0.03 | 0.73 | 0.41 | 0.136 (leapfrog (NextGEQ)) | 3.287 (bitset window (4096)) |
| OptPFor | 11.89 | 8.87 | 0.17 | 1.12 | 0.76 | 0.169 (leapfrog (NextGEQ)) | 3.492 (bitset window (4096)) |
| Elias-Fano | 10.85 | 8.08 | 0.05 | 1.77 | 1.88 | 0.087 (leapfrog (NextGEQ)) | 3.559 (bitset window (4096)) |
| PEF | 10.90 | 8.08 | 0.15 | 1.76 | 1.62 | 0.083 (leapfrog (NextGEQ)) | 3.525 (bitset window (4096)) |
| Interpolative | 11.33 | 8.52 | 0.15 | 3.60 | 3.83 | 0.424 (leapfrog (NextGEQ)) | 5.571 (bitset window (4096)) |
| Hybrid (bitmap + BP128) | 12.33 | 9.31 | 0.06 | 1.18 | 0.93 | 0.127 (native) | 2.047 (native) |
| Roaring | 20.42 | 13.70 | 0.20 | 2.17 | 0.37 | 0.035 (native) | 0.461 (native) |
| Trie | 20.10 | 13.24 | 1.15 | 17.42 | 10.61 | 0.323 (native) | 3.499 (native) |
| Run-Trie | 20.10 | 13.24 | 1.19 | 23.43 | 13.40 | 0.340 (native) | 3.889 (native) |
| Roaring (roaring crate) | 20.42 | 13.70 | 0.27 | 7.68 | 6.22 | 0.141 (leapfrog (NextGEQ)) | 4.816 (native) |

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.597 |
| Uncompressed | AND | SvS + merge (branchless) | 1.862 |
| Uncompressed | AND | SvS + galloping | 0.111 |
| Uncompressed | AND | SvS + binary search | 0.350 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.178 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.776 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.758 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.594 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.100 |
| Uncompressed | AND | Small Adaptive | 0.124 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.222 |
| Uncompressed | OR | slices: heap | 8.776 |
| Uncompressed | OR | slices: fold (pairwise merge) | 2.885 |
| Uncompressed | OR | slices: bitmap | 2.051 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.120 |
| Uncompressed | AND | round-robin (LFTJ) | 0.212 |
| Uncompressed | AND | decode + SvS | 0.298 |
| Uncompressed | OR | heap | 4.498 |
| Uncompressed | OR | min-scan | 3.872 |
| Uncompressed | OR | bitset window (4096) | 2.416 |
| VByte | AND | leapfrog (NextGEQ) | 0.271 |
| VByte | AND | round-robin (LFTJ) | 0.459 |
| VByte | AND | decode + SvS | 1.000 |
| VByte | OR | heap | 5.774 |
| VByte | OR | min-scan | 4.486 |
| VByte | OR | bitset window (4096) | 3.633 |
| BP128 | AND | leapfrog (NextGEQ) | 0.156 |
| BP128 | AND | round-robin (LFTJ) | 0.254 |
| BP128 | AND | decode + SvS | 0.509 |
| BP128 | OR | heap | 5.225 |
| BP128 | OR | min-scan | 3.614 |
| BP128 | OR | bitset window (4096) | 3.403 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.136 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.214 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.360 |
| BP128 (bitpacking crate) | OR | heap | 5.051 |
| BP128 (bitpacking crate) | OR | min-scan | 3.542 |
| BP128 (bitpacking crate) | OR | bitset window (4096) | 3.287 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.169 |
| OptPFor | AND | round-robin (LFTJ) | 0.283 |
| OptPFor | AND | decode + SvS | 0.612 |
| OptPFor | OR | heap | 5.326 |
| OptPFor | OR | min-scan | 3.606 |
| OptPFor | OR | bitset window (4096) | 3.492 |
| Elias-Fano | AND | leapfrog (NextGEQ) | 0.087 |
| Elias-Fano | AND | round-robin (LFTJ) | 0.140 |
| Elias-Fano | AND | decode + SvS | 1.252 |
| Elias-Fano | OR | heap | 5.577 |
| Elias-Fano | OR | min-scan | 4.009 |
| Elias-Fano | OR | bitset window (4096) | 3.559 |
| PEF | AND | leapfrog (NextGEQ) | 0.083 |
| PEF | AND | round-robin (LFTJ) | 0.135 |
| PEF | AND | decode + SvS | 1.069 |
| PEF | OR | heap | 6.194 |
| PEF | OR | min-scan | 4.339 |
| PEF | OR | bitset window (4096) | 3.525 |
| Interpolative | AND | leapfrog (NextGEQ) | 0.424 |
| Interpolative | AND | round-robin (LFTJ) | 0.791 |
| Interpolative | AND | decode + SvS | 2.686 |
| Interpolative | OR | heap | 8.068 |
| Interpolative | OR | min-scan | 5.744 |
| Interpolative | OR | bitset window (4096) | 5.571 |
| Hybrid (bitmap + BP128) | AND | native | 0.127 |
| Hybrid (bitmap + BP128) | OR | native | 2.047 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.154 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.208 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.983 |
| Hybrid (bitmap + BP128) | OR | heap | 7.215 |
| Hybrid (bitmap + BP128) | OR | min-scan | 6.249 |
| Hybrid (bitmap + BP128) | OR | bitset window (4096) | 6.267 |
| Roaring | AND | native | 0.035 |
| Roaring | OR | native | 0.461 |
| Roaring | AND | native, count only | 0.036 |
| Roaring | OR | native, count only | 0.284 |
| Roaring | AND | leapfrog (NextGEQ) | 0.070 |
| Roaring | AND | round-robin (LFTJ) | 0.115 |
| Roaring | AND | decode + SvS | 0.294 |
| Roaring | OR | heap | 5.561 |
| Roaring | OR | min-scan | 4.141 |
| Roaring | OR | bitset window (4096) | 3.313 |
| Trie | AND | native | 0.323 |
| Trie | OR | native | 3.499 |
| Trie | AND | leapfrog (NextGEQ) | 0.455 |
| Trie | AND | round-robin (LFTJ) | 0.733 |
| Trie | AND | decode + SvS | 2.102 |
| Trie | OR | heap | 9.394 |
| Trie | OR | min-scan | 6.922 |
| Trie | OR | bitset window (4096) | 7.267 |
| Run-Trie | AND | native | 0.340 |
| Run-Trie | OR | native | 3.889 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.511 |
| Run-Trie | AND | round-robin (LFTJ) | 0.753 |
| Run-Trie | AND | decode + SvS | 2.575 |
| Run-Trie | OR | heap | 9.982 |
| Run-Trie | OR | min-scan | 8.084 |
| Run-Trie | OR | bitset window (4096) | 8.143 |
| Roaring (roaring crate) | AND | native | 0.251 |
| Roaring (roaring crate) | OR | native | 4.816 |
| Roaring (roaring crate) | AND | native, count only | 0.226 |
| Roaring (roaring crate) | OR | native, count only | 0.573 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.141 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.205 |
| Roaring (roaring crate) | AND | decode + SvS | 4.223 |
| Roaring (roaring crate) | OR | heap | 7.149 |
| Roaring (roaring crate) | OR | min-scan | 6.708 |
| Roaring (roaring crate) | OR | bitset window (4096) | 5.846 |
