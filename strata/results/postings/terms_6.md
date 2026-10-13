
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 6.00 terms, 735851 postings) |
| mean AND / OR result | 2 / 686811 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.05 | 0.28 | 0.23 | 0.013 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.80 | 0.05 | 0.90 | 0.71 | 0.027 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.80 | 0.03 | 0.62 | 0.42 | 0.019 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.98 | 0.21 | 1.31 | 0.81 | 0.023 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 5.03 | 0.16 | 1.71 | 1.39 | 0.014 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.69 | 0.05 | 1.04 | 0.87 | 0.019 (native) | — |
| Roaring | 11.72 | 9.43 | 0.13 | 2.51 | 1.77 | 0.011 (native) | — |
| Trie | 9.61 | 7.28 | 0.70 | 7.61 | 5.10 | 0.037 (native) | — |
| Run-Trie | 9.13 | 6.78 | 0.68 | 9.69 | 7.02 | 0.043 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.43 | 0.26 | 7.81 | 7.14 | 0.038 (leapfrog (NextGEQ)) | — |

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.356 |
| Uncompressed | AND | SvS + merge (branchless) | 1.181 |
| Uncompressed | AND | SvS + galloping | 0.015 |
| Uncompressed | AND | SvS + binary search | 0.070 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.022 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.483 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.482 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.382 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.013 |
| Uncompressed | AND | Small Adaptive | 0.025 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.102 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.016 |
| Uncompressed | AND | round-robin (LFTJ) | 0.095 |
| Uncompressed | AND | decode + SvS | 0.242 |
| BP128 | AND | leapfrog (NextGEQ) | 0.027 |
| BP128 | AND | round-robin (LFTJ) | 0.086 |
| BP128 | AND | decode + SvS | 0.495 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.019 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.071 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.331 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.023 |
| OptPFor | AND | round-robin (LFTJ) | 0.091 |
| OptPFor | AND | decode + SvS | 0.633 |
| PEF | AND | leapfrog (NextGEQ) | 0.014 |
| PEF | AND | round-robin (LFTJ) | 0.056 |
| PEF | AND | decode + SvS | 0.969 |
| Hybrid (bitmap + BP128) | AND | native | 0.019 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.021 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.071 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.844 |
| Roaring | AND | native | 0.011 |
| Roaring | AND | native, count only | 0.011 |
| Roaring | AND | leapfrog (NextGEQ) | 0.013 |
| Roaring | AND | round-robin (LFTJ) | 0.054 |
| Roaring | AND | decode + SvS | 0.547 |
| Trie | AND | native | 0.037 |
| Trie | AND | leapfrog (NextGEQ) | 0.060 |
| Trie | AND | round-robin (LFTJ) | 0.210 |
| Trie | AND | decode + SvS | 2.032 |
| Run-Trie | AND | native | 0.043 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.068 |
| Run-Trie | AND | round-robin (LFTJ) | 0.231 |
| Run-Trie | AND | decode + SvS | 2.503 |
| Roaring (roaring crate) | AND | native | 0.050 |
| Roaring (roaring crate) | AND | native, count only | 0.050 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.038 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.100 |
| Roaring (roaring crate) | AND | decode + SvS | 4.690 |
