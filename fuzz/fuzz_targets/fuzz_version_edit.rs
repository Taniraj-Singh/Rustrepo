#![no_main]
//! Fuzzes the manifest version-edit decoder. Arbitrary bytes must decode to a
//! valid edit or a structured error. A successfully decoded edit is re-encoded
//! and re-decoded to check the codec is self-consistent.

use libfuzzer_sys::fuzz_target;

use cairn::version::VersionEdit;

fuzz_target!(|data: &[u8]| {
    if let Ok(edit) = VersionEdit::decode(data) {
        // Re-encoding a decoded edit and decoding again must succeed.
        let reencoded = edit.encode();
        let _ = VersionEdit::decode(&reencoded).expect("re-decode of encoded edit");
    }
});
