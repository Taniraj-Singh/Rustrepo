#![no_main]
//! Fuzzes the fixed table footer decoder. Arbitrary bytes must decode to a valid
//! footer or a structured error — never a panic.

use libfuzzer_sys::fuzz_target;

use cairn::table::Footer;

fuzz_target!(|data: &[u8]| {
    let _ = Footer::decode(data);
});
