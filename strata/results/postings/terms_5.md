
## Collection and workload

| | |
|---|---|
| collection | synthetic-clustered (u=8000000, terms=20000, max_len=2000000, zipf=1) |
| documents | 8000000 |
| lists / postings | 20000 / 20951573 |
| queries | 300 (mean 5.00 terms, 763061 postings) |
| mean AND / OR result | 6 / 720237 docs |
| CPU | aarch64 (neon) |

## Summary

| codec | bits/posting | bits/posting (query lists) | aux memory MB | build s | decode ns/posting | decode ns/posting (query lists) | best AND ms | best OR ms |
|---|---:|---:|---:|---:|---:|---:|---|---|
| Uncompressed | 32.00 | 32.00 | 0.0 | 0.01 | 0.08 | 0.08 | 0.008 (SvS + adaptive (SIMD / gallop @64x)) | — |
| BP128 | 10.07 | 7.74 | 0.0 | 0.03 | 0.88 | 0.77 | 0.022 (leapfrog (NextGEQ)) | — |
| BP128 (bitpacking crate) | 10.07 | 7.74 | 0.0 | 0.01 | 0.44 | 0.30 | 0.018 (leapfrog (NextGEQ)) | — |
| OptPFor | 7.13 | 5.95 | 0.0 | 0.14 | 1.14 | 0.89 | 0.023 (leapfrog (NextGEQ)) | — |
| PEF | 7.04 | 4.98 | 0.0 | 0.18 | 1.12 | 1.01 | 0.015 (leapfrog (NextGEQ)) | — |
| Hybrid (bitmap + BP128) | 10.01 | 7.63 | 0.0 | 0.03 | 0.94 | 0.85 | 0.022 (native) | — |
| Roaring | 11.72 | 9.36 | 0.0 | 0.08 | 1.35 | 0.96 | 0.010 (native) | — |
| Trie | 9.61 | 7.23 | 0.0 | 0.39 | 10.63 | 7.98 | 0.042 (native) | — |
| Run-Trie | 9.13 | 6.72 | 0.0 | 0.43 | 11.49 | 8.66 | 0.043 (native) | — |
| Roaring (roaring crate) | 11.72 | 9.36 | 0.0 | 0.16 | 5.64 | 5.22 | 0.031 (leapfrog (NextGEQ)) | — |

## Pareto frontier (bits/posting on query lists vs best ms/query)

* AND: PEF (4.98 bits, 0.015 ms) → Roaring (9.36 bits, 0.010 ms) → Uncompressed (32.00 bits, 0.008 ms)
* OR: 

## All measurements (ms per query)

| codec | op | algorithm | ms/query |
|---|---|---|---:|
| Uncompressed | AND | SvS + merge | 0.350 |
| Uncompressed | AND | SvS + merge (branchless) | 0.465 |
| Uncompressed | AND | SvS + galloping | 0.011 |
| Uncompressed | AND | SvS + binary search | 0.066 |
| Uncompressed | AND | SvS + Baeza-Yates | 0.016 |
| Uncompressed | AND | SvS + SIMD NEON 4x4 | 0.253 |
| Uncompressed | AND | SvS + SIMD NEON 8x8 | 0.209 |
| Uncompressed | AND | SvS + adaptive (SIMD / gallop @64x) | 0.008 |
| Uncompressed | AND | Small Adaptive | 0.021 |
| Uncompressed | AND | Sequential (Barbay-Kenyon) | 0.050 |
| Uncompressed | AND | leapfrog (NextGEQ) | 0.010 |
| Uncompressed | AND | round-robin (LFTJ) | 0.048 |
| Uncompressed | AND | decode + SvS | 0.060 |
| BP128 | AND | leapfrog (NextGEQ) | 0.022 |
| BP128 | AND | round-robin (LFTJ) | 0.083 |
| BP128 | AND | decode + SvS | 0.585 |
| BP128 (bitpacking crate) | AND | leapfrog (NextGEQ) | 0.018 |
| BP128 (bitpacking crate) | AND | round-robin (LFTJ) | 0.053 |
| BP128 (bitpacking crate) | AND | decode + SvS | 0.228 |
| OptPFor | AND | leapfrog (NextGEQ) | 0.023 |
| OptPFor | AND | round-robin (LFTJ) | 0.085 |
| OptPFor | AND | decode + SvS | 0.663 |
| PEF | AND | leapfrog (NextGEQ) | 0.015 |
| PEF | AND | round-robin (LFTJ) | 0.043 |
| PEF | AND | decode + SvS | 0.749 |
| Hybrid (bitmap + BP128) | AND | native | 0.022 |
| Hybrid (bitmap + BP128) | AND | leapfrog (NextGEQ) | 0.022 |
| Hybrid (bitmap + BP128) | AND | round-robin (LFTJ) | 0.066 |
| Hybrid (bitmap + BP128) | AND | decode + SvS | 0.755 |
| Roaring | AND | native | 0.010 |
| Roaring | AND | native, count only | 0.010 |
| Roaring | AND | leapfrog (NextGEQ) | 0.010 |
| Roaring | AND | round-robin (LFTJ) | 0.030 |
| Roaring | AND | decode + SvS | 0.410 |
| Trie | AND | native | 0.042 |
| Trie | AND | leapfrog (NextGEQ) | 0.079 |
| Trie | AND | round-robin (LFTJ) | 0.196 |
| Trie | AND | decode + SvS | 4.214 |
| Run-Trie | AND | native | 0.043 |
| Run-Trie | AND | leapfrog (NextGEQ) | 0.082 |
| Run-Trie | AND | round-robin (LFTJ) | 0.206 |
| Run-Trie | AND | decode + SvS | 4.546 |
| Roaring (roaring crate) | AND | native | 0.044 |
| Roaring (roaring crate) | AND | native, count only | 0.044 |
| Roaring (roaring crate) | AND | leapfrog (NextGEQ) | 0.031 |
| Roaring (roaring crate) | AND | round-robin (LFTJ) | 0.063 |
| Roaring (roaring crate) | AND | decode + SvS | 3.242 |
