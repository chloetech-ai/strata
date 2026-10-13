
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 3.00 terms, 684896 postings) |
| mean AND / OR result | 111 / 669995 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.06 | 0.32 | 0.26 | 0.047 (SvS + galloping) | — |
| BP128 | 10.07 | 7.51 | 0.05 | 0.86 | 0.64 | 0.059 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.51 | 0.03 | 0.63 | 0.42 | 0.059 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.81 | 0.22 | 1.17 | 0.76 | 0.062 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.77 | 0.17 | 1.73 | 1.38 | 0.037 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.39 | 0.05 | 1.05 | 0.94 | 0.061 (native) | — |
| Roaring | 11.72 | 9.17 | 0.14 | 2.57 | 1.71 | 0.028 (native) | — |
| Trie | 9.61 | 6.96 | 0.65 | 7.59 | 4.68 | 0.114 (native) | — |
| Run-Trie | 9.13 | 6.46 | 0.68 | 9.19 | 5.99 | 0.113 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.17 | 0.26 | 8.58 | 7.34 | 0.073 (leapfrog (NextGEQ)) | — |

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.574 |
| Uncompressed | AND | SvS + merge (branchless) | 1.854 |
| Uncompressed | AND | SvS + galloping | 0.047 |
| Uncompressed | AND | SvS + binary search | 0.236 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.067 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.765 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.752 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.652 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.054 |
| Uncompressed | AND | Small Adaptive | 0.057 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.128 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.047 |
| Uncompressed | AND | round-robin (LFTJ) | 0.124 |
| Uncompressed | AND | decode + SvS | 0.256 |
| BP128 | AND | leapfrog (NextGEQ) | 0.059 |
| BP128 | AND | round-robin (LFTJ) | 0.110 |
| BP128 | AND | decode + SvS | 0.485 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.059 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.102 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.343 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.062 |
| OptPFor | AND | round-robin (LFTJ) | 0.141 |
| OptPFor | AND | decode + SvS | 0.612 |
| PEF | AND | leapfrog (NextGEQ) | 0.037 |
| PEF | AND | round-robin (LFTJ) | 0.066 |
| PEF | AND | decode + SvS | 0.929 |
| Hybrid (bitmap + BP128) | AND | native | 0.061 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.061 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.090 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.924 |
| Roaring | AND | native | 0.028 |
| Roaring | AND | native, count only | 0.028 |
| Roaring | AND | leapfrog (NextGEQ) | 0.035 |
| Roaring | AND | round-robin (LFTJ) | 0.059 |
| Roaring | AND | decode + SvS | 0.435 |
| Trie | AND | native | 0.114 |
| Trie | AND | leapfrog (NextGEQ) | 0.169 |
| Trie | AND | round-robin (LFTJ) | 0.275 |
| Trie | AND | decode + SvS | 1.532 |
| Run-Trie | AND | native | 0.113 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.177 |
| Run-Trie | AND | round-robin (LFTJ) | 0.267 |
| Run-Trie | AND | decode + SvS | 1.985 |
| Roaring (roaring crate) | AND | native | 0.095 |
| Roaring (roaring crate) | AND | native, count only | 0.097 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.073 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.101 |
| Roaring (roaring crate) | AND | decode + SvS | 4.195 |
