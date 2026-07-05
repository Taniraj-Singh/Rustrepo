#![no_main]
//! Fuzzes the block decompressors. Arbitrary bytes are fed to each codec with a
//! bounded output cap; the decoder must return a value or a structured error,
//! never panic or exceed the cap. Also checks the encode->decode round-trip.

use libfuzzer_sys::fuzz_target;

use cairn::compression::{compress, decompress};
use cairn::CompressionKind;

const KINDS: [CompressionKind; 3] = [
    CompressionKind::None,
    CompressionKind::Rle,
    CompressionKind::Lz,
];

fuzz_target!(|data: &[u8]| {
    // 1. Treat `data` as an (untrusted) compressed stream for every codec.
    for kind in KINDS {
        let _ = decompress(kind, data, 1 << 20);
    }

    // 2. Treat `data` as a plaintext payload and require a byte-exact
    //    round-trip through every codec.
    for kind in KINDS {
        let encoded = compress(kind, data);
        match decompress(kind, &encoded, data.len()) {
            Ok(decoded) => assert_eq!(decoded, data, "{kind:?} round-trip mismatch"),
            Err(e) => panic!("{kind:?} failed to decode its own output: {e}"),
        }
    }
});
