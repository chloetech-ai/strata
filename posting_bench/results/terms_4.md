
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 4.00 terms, 875849 postings) |
| mean AND / OR result | 24 / 836333 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.05 | 0.29 | 0.24 | 0.024 (leapfrog (NextGEQ)) | — |
| BP128 | 10.07 | 7.61 | 0.05 | 0.79 | 0.58 | 0.039 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.61 | 0.03 | 0.60 | 0.39 | 0.035 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.88 | 0.21 | 1.36 | 0.98 | 0.040 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.86 | 0.16 | 1.71 | 1.37 | 0.023 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.49 | 0.05 | 1.01 | 0.83 | 0.033 (leapfrog (NextGEQ)) | — |
| Roaring | 11.72 | 9.25 | 0.13 | 2.44 | 1.65 | 0.019 (native) | — |
| Trie | 9.61 | 7.09 | 0.66 | 7.69 | 4.69 | 0.073 (native) | — |
| Run-Trie | 9.13 | 6.58 | 0.66 | 8.76 | 6.28 | 0.069 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.25 | 0.24 | 7.81 | 7.03 | 0.050 (leapfrog (NextGEQ)) | — |

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.549 |
| Uncompressed | AND | SvS + merge (branchless) | 1.672 |
| Uncompressed | AND | SvS + galloping | 0.028 |
| Uncompressed | AND | SvS + binary search | 0.148 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.054 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.690 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.676 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.496 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.029 |
| Uncompressed | AND | Small Adaptive | 0.034 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.110 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.024 |
| Uncompressed | AND | round-robin (LFTJ) | 0.100 |
| Uncompressed | AND | decode + SvS | 0.274 |
| BP128 | AND | leapfrog (NextGEQ) | 0.039 |
| BP128 | AND | round-robin (LFTJ) | 0.106 |
| BP128 | AND | decode + SvS | 0.590 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.035 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.081 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.373 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.040 |
| OptPFor | AND | round-robin (LFTJ) | 0.110 |
| OptPFor | AND | decode + SvS | 0.756 |
| PEF | AND | leapfrog (NextGEQ) | 0.023 |
| PEF | AND | round-robin (LFTJ) | 0.069 |
| PEF | AND | decode + SvS | 1.153 |
| Hybrid (bitmap + BP128) | AND | native | 0.033 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.033 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.086 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 1.185 |
| Roaring | AND | native | 0.019 |
| Roaring | AND | native, count only | 0.019 |
| Roaring | AND | leapfrog (NextGEQ) | 0.021 |
| Roaring | AND | round-robin (LFTJ) | 0.057 |
| Roaring | AND | decode + SvS | 0.518 |
| Trie | AND | native | 0.073 |
| Trie | AND | leapfrog (NextGEQ) | 0.107 |
| Trie | AND | round-robin (LFTJ) | 0.261 |
| Trie | AND | decode + SvS | 2.083 |
| Run-Trie | AND | native | 0.069 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.101 |
| Run-Trie | AND | round-robin (LFTJ) | 0.262 |
| Run-Trie | AND | decode + SvS | 2.419 |
| Roaring (roaring crate) | AND | native | 0.078 |
| Roaring (roaring crate) | AND | native, count only | 0.077 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.050 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.095 |
| Roaring (roaring crate) | AND | decode + SvS | 5.094 |
