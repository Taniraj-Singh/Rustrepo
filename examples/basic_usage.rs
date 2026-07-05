//! Basic usage: open a database, do CRUD, and scan.
//!
//! Run with:
//!
//! ```text
//! cargo run --example basic_usage
//! ```

use cairn::{Cairn, Config};

fn main() -> cairn::Result<()> {
    // Use a scratch directory that we clean up at the end.
    let dir = std::env::temp_dir().join(format!("cairn-example-basic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let db = Cairn::open(&dir, Config::default())?;

    // --- put / get ---
    db.put(b"name", b"Cairn")?;
    db.put(b"kind", b"LSM key-value store")?;
    db.put(b"language", b"Rust")?;

    println!("name     = {}", show(db.get(b"name")?));
    println!("kind     = {}", show(db.get(b"kind")?));
    println!("missing  = {}", show(db.get(b"missing")?));

    // --- overwrite ---
    db.put(b"language", b"safe Rust")?;
    println!("language = {}", show(db.get(b"language")?));

    // --- delete ---
    db.delete(b"kind")?;
    println!("kind     = {} (after delete)", show(db.get(b"kind")?));

    // --- ordered scan ---
    println!("\nfull scan (sorted by key):");
    let mut it = db.scan()?;
    it.seek_to_first();
    while it.valid() {
        println!(
            "  {} -> {}",
            String::from_utf8_lossy(it.key()),
            String::from_utf8_lossy(it.value())
        );
        it.next();
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn show(v: Option<Vec<u8>>) -> String {
    match v {
        Some(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        None => "<none>".to_string(),
    }
}
