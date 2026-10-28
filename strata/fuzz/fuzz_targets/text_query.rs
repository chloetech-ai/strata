#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| strata_fuzz::text_query(data));
