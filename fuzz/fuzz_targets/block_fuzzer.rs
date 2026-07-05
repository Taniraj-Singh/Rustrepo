#![no_main]
//! Fuzzes the table data-block parser and iterator. Arbitrary bytes are parsed
//! as a block; if parsing succeeds, the block is fully iterated and seeked. No
//! input may cause a panic or an unbounded loop.

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;

use cairn::table::Block;
use cairn::KvIterator;

fuzz_target!(|data: &[u8]| {
    let bytes: Arc<[u8]> = Arc::from(data.to_vec().into_boxed_slice());
    if let Ok(block) = Block::new(bytes) {
        let mut it = block.iter();

        // Full forward scan, bounded so a malformed block cannot loop forever.
        it.seek_to_first();
        let mut steps = 0;
        while it.valid() && steps < 100_000 {
            let _ = it.key();
            let _ = it.value();
            it.next();
            steps += 1;
        }
        let _ = it.status();

        // Seek to a few probe keys derived from the input.
        if !data.is_empty() {
            it.seek(&data[..data.len().min(16)]);
            let _ = it.valid();
        }
        it.seek(&[]);
        let _ = it.valid();
    }
});
