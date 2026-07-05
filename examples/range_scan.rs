//! Range scans: iterate a contiguous slice of the keyspace with `seek`.
//!
//! ```text
//! cargo run --example range_scan
//! ```

use cairn::{Cairn, Config};

fn main() -> cairn::Result<()> {
    let dir = std::env::temp_dir().join(format!("cairn-example-range-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Cairn::open(&dir, Config::default())?;

    // Insert user records keyed "user:0000".."user:0099".
    for i in 0..100u32 {
        db.put(
            format!("user:{i:04}").as_bytes(),
            format!("profile #{i}").as_bytes(),
        )?;
    }
    // Some unrelated keys that must not appear in the range below.
    db.put(b"session:abc", b"...")?;
    db.put(b"zzz-last", b"...")?;

    // Scan the half-open range [user:0010, user:0020).
    let start = b"user:0010";
    let end = b"user:0020";

    println!("range scan [{}, {}):", s(start), s(end));
    let mut it = db.scan()?;
    it.seek(start);
    while it.valid() && it.key() < &end[..] {
        println!("  {} -> {}", s(it.key()), s(it.value()));
        it.next();
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn s(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}
