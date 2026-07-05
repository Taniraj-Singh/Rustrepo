//! Atomic write batches and MVCC snapshots.
//!
//! Demonstrates that a [`WriteBatch`] applies all-or-nothing and that a
//! [`Snapshot`](cairn::Snapshot) gives a stable read view even as later writes
//! land.
//!
//! ```text
//! cargo run --example batch_and_snapshot
//! ```

use cairn::{Cairn, Config, WriteBatch};

fn main() -> cairn::Result<()> {
    let dir = std::env::temp_dir().join(format!("cairn-example-batch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Cairn::open(&dir, Config::default())?;

    // --- atomic batch ---
    let mut batch = WriteBatch::new();
    batch.put(b"account:alice", b"100");
    batch.put(b"account:bob", b"50");
    batch.put(b"account:carol", b"75");
    println!("committing a batch of {} operations", batch.count());
    db.write(batch)?;

    // --- a snapshot pins the current state ---
    let snapshot = db.snapshot();
    println!("\ntook a snapshot at sequence {}", snapshot.sequence());

    // Transfer 30 from alice to bob in one atomic batch.
    let mut transfer = WriteBatch::new();
    transfer.put(b"account:alice", b"70");
    transfer.put(b"account:bob", b"80");
    db.write(transfer)?;

    println!("\nafter the transfer:");
    println!("  alice (live)     = {}", read(&db, b"account:alice")?);
    println!("  bob   (live)     = {}", read(&db, b"account:bob")?);

    println!("\nas seen through the earlier snapshot:");
    println!(
        "  alice (snapshot) = {}",
        String::from_utf8_lossy(&db.get_at(b"account:alice", &snapshot)?.unwrap())
    );
    println!(
        "  bob   (snapshot) = {}",
        String::from_utf8_lossy(&db.get_at(b"account:bob", &snapshot)?.unwrap())
    );

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn read(db: &Cairn, key: &[u8]) -> cairn::Result<String> {
    Ok(db
        .get(key)?
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .unwrap_or_else(|| "<none>".to_string()))
}
