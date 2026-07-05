//! A tiny, dependency-free leveled logger.
//!
//! The engine emits diagnostics through the [`log_at!`](crate::log_at) macro,
//! which formats lazily: when the active level filters a message out, its
//! arguments are never formatted, so logging costs nothing on hot paths when
//! disabled. Output goes to standard error.
//!
//! This is deliberately minimal — no timestamps, no async, no `log` crate
//! facade — because a storage engine should not drag a logging framework into
//! its dependency tree. Applications that want structured logging can raise the
//! level to [`Level::Off`] and observe [`crate::util::metrics`] instead.

use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};

/// Severity of a log message, ordered from most to least severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    /// Logging disabled entirely.
    Off = 0,
    /// Unrecoverable or data-threatening conditions.
    Error = 1,
    /// Recoverable anomalies worth an operator's attention.
    Warn = 2,
    /// High-level lifecycle events (open, flush, compaction).
    Info = 3,
    /// Detailed internal events.
    Debug = 4,
    /// Extremely verbose tracing.
    Trace = 5,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Off => "OFF",
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

// The default level is `Warn`: quiet in normal operation, but surfaces
// anomalies. Stored as a plain atomic so it can be changed at runtime without a
// lock.
static ACTIVE_LEVEL: AtomicU8 = AtomicU8::new(Level::Warn as u8);

/// Sets the global maximum level that will be emitted.
pub fn set_level(level: Level) {
    ACTIVE_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Returns the current global level.
pub fn level() -> Level {
    match ACTIVE_LEVEL.load(Ordering::Relaxed) {
        0 => Level::Off,
        1 => Level::Error,
        2 => Level::Warn,
        3 => Level::Info,
        4 => Level::Debug,
        _ => Level::Trace,
    }
}

/// Whether a message at `level` would currently be emitted. Used by the
/// [`log_at!`](crate::log_at) macro to skip formatting when filtered.
#[inline]
pub fn enabled(level: Level) -> bool {
    level != Level::Off && (level as u8) <= ACTIVE_LEVEL.load(Ordering::Relaxed)
}

/// Emits a pre-formatted message. Prefer the [`log_at!`](crate::log_at) macro,
/// which checks [`enabled`] first.
pub fn emit(level: Level, args: fmt::Arguments<'_>) {
    eprintln!("[cairn {}] {}", level.label(), args);
}

/// Logs at an explicit level, formatting lazily.
///
/// ```
/// use cairn::util::logging::{self, Level};
/// logging::set_level(Level::Debug);
/// cairn::log_at!(Level::Info, "opened {} tables", 3);
/// ```
#[macro_export]
macro_rules! log_at {
    ($level:expr, $($arg:tt)*) => {{
        let lvl = $level;
        if $crate::util::logging::enabled(lvl) {
            $crate::util::logging::emit(lvl, ::std::format_args!($($arg)*));
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_ordering() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Trace > Level::Info);
    }

    #[test]
    fn enabled_respects_active_level() {
        set_level(Level::Warn);
        assert!(enabled(Level::Error));
        assert!(enabled(Level::Warn));
        assert!(!enabled(Level::Info));
        assert!(!enabled(Level::Trace));

        set_level(Level::Off);
        assert!(!enabled(Level::Error));

        set_level(Level::Trace);
        assert!(enabled(Level::Trace));

        // Restore the default so other tests see a predictable level.
        set_level(Level::Warn);
    }

    #[test]
    fn round_trips_through_u8() {
        for lvl in [
            Level::Off,
            Level::Error,
            Level::Warn,
            Level::Info,
            Level::Debug,
            Level::Trace,
        ] {
            set_level(lvl);
            assert_eq!(level(), lvl);
        }
        set_level(Level::Warn);
    }
}
