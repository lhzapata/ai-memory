//! Store-level error type.

use thiserror::Error;

use ai_memory_core::MemoryError;

/// Result alias used throughout the store crate.
pub type StoreResult<T> = Result<T, StoreError>;

/// Errors raised by the store layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Underlying SQLite error.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Migration runner failed.
    #[error("migration: {0}")]
    Migration(#[from] refinery::Error),

    /// I/O failed (e.g. opening the DB file).
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON serialisation failure (frontmatter).
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    /// Writer actor has shut down.
    #[error("writer actor is no longer running")]
    WriterClosed,

    /// A `spawn_blocking` task panicked or was cancelled.
    #[error("reader pool task did not complete: {0}")]
    PoolPanic(String),

    /// Re-export of [`MemoryError`] for cross-crate propagation.
    #[error(transparent)]
    Memory(#[from] MemoryError),
}
