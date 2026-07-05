//! Cross-cutting utilities: integer coding, key ordering, byte helpers,
//! logging, and metrics.
//!
//! Nothing here knows about the LSM engine; these are the primitives the rest
//! of the crate is built from. Keeping them in one place — and dependency-free
//! — means the engine's trust surface is auditable in isolation.

pub mod bytes;
pub mod coding;
pub mod comparator;
pub mod histogram;
pub mod logging;
pub mod metrics;
pub mod varint;

pub use comparator::{BytewiseComparator, Comparator, InternalKeyComparator};
pub use histogram::Histogram;
pub use logging::Level;
pub use metrics::{Metrics, MetricsSnapshot};
