#![no_main]
//! Fuzzes the write-ahead-log reader. Arbitrary bytes are walked as a log; the
//! reader must terminate at a truncated tail or a corrupt record without
//! panicking or looping.

use libfuzzer_sys::fuzz_target;

use cairn::storage::LogReader;

fuzz_target!(|data: &[u8]| {
    let mut reader = LogReader::new(data);
    // Bound the loop; a well-formed reader always makes progress or stops.
    for _ in 0..100_000 {
        match reader.read_record() {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => break,
        }
    }
});
