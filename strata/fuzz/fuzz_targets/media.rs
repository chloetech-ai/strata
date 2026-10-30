#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| strata_fuzz::media(data));
