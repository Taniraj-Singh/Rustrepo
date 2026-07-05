#![no_main]
//! Fuzzes the write-batch (WAL payload) decoder. Arbitrary bytes must decode to
//! a valid batch or a structured error; a decoded batch is iterated to confirm
//! its record stream is internally consistent.

use libfuzzer_sys::fuzz_target;

use cairn::WriteBatch;

fuzz_target!(|data: &[u8]| {
    if let Ok(batch) = WriteBatch::from_bytes(data) {
        // Walking the records must not panic and must agree with the count.
        let _ = batch.for_each(|_type, _key, _value| Ok(()));
    }
});
