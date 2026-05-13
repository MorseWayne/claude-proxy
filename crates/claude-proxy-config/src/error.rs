use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse config file {}: {message}", .path.display())]
    Parse { path: PathBuf, message: String },

    #[error("invalid configuration: {0}")]
    Validation(String),

    #[error("migration failed: {0}")]
    Migration(String),
}
