
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 5.00 terms, 763061 postings) |
| mean AND / OR result | 6 / 720237 docs |
| CPU | Intel(R) Xeon(R) Processor @ 2.10GHz (sse4.2, avx2, bmi2, avx512f, avx512bw) |

## Summary

| codec | bits/posting | bits/posting (query lists) | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.05 | 0.21 | 0.21 | 0.019 (leapfrog (NextGEQ)) | — |
| BP128 | 10.07 | 7.74 | 0.05 | 0.93 | 0.72 | 0.029 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.74 | 0.03 | 0.60 | 0.41 | 0.025 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.95 | 0.22 | 1.28 | 0.83 | 0.029 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.98 | 0.16 | 1.65 | 1.44 | 0.019 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.63 | 0.05 | 1.41 | 0.88 | 0.025 (native) | — |
| Roaring | 11.72 | 9.36 | 0.13 | 2.43 | 1.59 | 0.016 (native) | — |
| Trie | 9.61 | 7.23 | 0.62 | 7.63 | 4.99 | 0.055 (native) | — |
| Run-Trie | 9.13 | 6.72 | 0.74 | 9.37 | 6.07 | 0.058 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.36 | 0.25 | 7.71 | 6.85 | 0.046 (leapfrog (NextGEQ)) | — |

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.388 |
| Uncompressed | AND | SvS + merge (branchless) | 1.426 |
| Uncompressed | AND | SvS + galloping | 0.022 |
| Uncompressed | AND | SvS + binary search | 0.113 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.034 |
| Uncompressed | AND | SvS + SIMD SSE 4x4 | 0.583 |
| Uncompressed | AND | SvS + SIMD AVX2 8x8 | 0.560 |
| Uncompressed | AND | SvS + SIMD AVX-512 16x16 | 0.459 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.020 |
| Uncompressed | AND | Small Adaptive | 0.032 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.108 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.019 |
| Uncompressed | AND | round-robin (LFTJ) | 0.099 |
| Uncompressed | AND | decode + SvS | 0.236 |
| BP128 | AND | leapfrog (NextGEQ) | 0.029 |
| BP128 | AND | round-robin (LFTJ) | 0.099 |
| BP128 | AND | decode + SvS | 0.524 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.025 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.078 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.343 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.029 |
| OptPFor | AND | round-robin (LFTJ) | 0.106 |
| OptPFor | AND | decode + SvS | 0.660 |
| PEF | AND | leapfrog (NextGEQ) | 0.019 |
| PEF | AND | round-robin (LFTJ) | 0.063 |
| PEF | AND | decode + SvS | 1.037 |
| Hybrid (bitmap + BP128) | AND | native | 0.025 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.027 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.080 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.901 |
| Roaring | AND | native | 0.016 |
| Roaring | AND | native, count only | 0.017 |
| Roaring | AND | leapfrog (NextGEQ) | 0.020 |
| Roaring | AND | round-robin (LFTJ) | 0.057 |
| Roaring | AND | decode + SvS | 0.539 |
| Trie | AND | native | 0.055 |
| Trie | AND | leapfrog (NextGEQ) | 0.083 |
| Trie | AND | round-robin (LFTJ) | 0.252 |
| Trie | AND | decode + SvS | 2.039 |
| Run-Trie | AND | native | 0.058 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.086 |
| Run-Trie | AND | round-robin (LFTJ) | 0.259 |
| Run-Trie | AND | decode + SvS | 2.499 |
| Roaring (roaring crate) | AND | native | 0.078 |
| Roaring (roaring crate) | AND | native, count only | 0.072 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.046 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.098 |
| Roaring (roaring crate) | AND | decode + SvS | 4.563 |
