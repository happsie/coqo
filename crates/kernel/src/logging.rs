// SPDX-License-Identifier: Apache-2.0

//! Structured logging setup for Coqo binaries (`coqod`, `coqo`).
//!
//! Everything in Coqo logs through [`tracing`]. Call [`init`] once at startup;
//! the filter comes from the `COQO_LOG` environment variable (same syntax as
//! `RUST_LOG`, e.g. `COQO_LOG=coqo_kernel=debug,info`) and falls back to
//! [`LogConfig::default_directive`].
//!
//! Never log secrets. Child process output is only logged at `trace` level,
//! under the `coqo_kernel::child_output` target.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

/// Environment variable read for the log filter.
pub const LOG_ENV: &str = "COQO_LOG";

/// Output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable single-line text.
    #[default]
    Text,
    /// One JSON object per line, for log files and tooling.
    Json,
}

/// Where logs go.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LogTarget {
    /// Standard error.
    #[default]
    Stderr,
    /// Appended to a file; parent directories are created. Use this for a
    /// detached daemon, which has no terminal.
    File(PathBuf),
}

/// Logging configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    /// Filter used when `COQO_LOG` is unset or invalid.
    pub default_directive: String,
    /// Output format.
    pub format: LogFormat,
    /// Destination.
    pub target: LogTarget,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            default_directive: "info".to_owned(),
            format: LogFormat::default(),
            target: LogTarget::default(),
        }
    }
}

/// Why logging could not be set up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LogInitError {
    /// The log file couldn't be opened.
    #[error("cannot open log file: {0}")]
    Io(#[from] io::Error),
    /// The default directive doesn't parse.
    #[error("invalid log filter: {0}")]
    Filter(#[from] tracing_subscriber::filter::ParseError),
    /// A global subscriber is already installed.
    #[error("logging is already initialised")]
    AlreadyInitialised,
}

/// Builds the filter: `COQO_LOG` if set and valid, otherwise `default_directive`.
pub fn env_filter(default_directive: &str) -> Result<EnvFilter, LogInitError> {
    match std::env::var(LOG_ENV) {
        Ok(value) if !value.trim().is_empty() => match EnvFilter::try_new(&value) {
            Ok(filter) => Ok(filter),
            Err(_) => Ok(EnvFilter::try_new(default_directive)?),
        },
        _ => Ok(EnvFilter::try_new(default_directive)?),
    }
}

/// Installs the global `tracing` subscriber described by `config`.
pub fn init(config: &LogConfig) -> Result<(), LogInitError> {
    let filter = env_filter(&config.default_directive)?;
    let (writer, ansi) = match &config.target {
        LogTarget::Stderr => (BoxMakeWriter::new(io::stderr), true),
        LogTarget::File(path) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            (BoxMakeWriter::new(Mutex::new(file)), false)
        }
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(ansi);
    let result = match config.format {
        LogFormat::Text => builder.try_init(),
        LogFormat::Json => builder.json().try_init(),
    };
    result.map_err(|_| LogInitError::AlreadyInitialised)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_filter_parses() {
        assert!(env_filter("info").is_ok());
        assert!(env_filter("coqo_kernel=debug,warn").is_ok());
        assert!(matches!(env_filter("=["), Err(LogInitError::Filter(_))));
    }

    #[test]
    fn init_writes_to_file_and_only_once() {
        let dir = std::env::temp_dir().join(format!("coqo-kernel-log-{}", std::process::id()));
        let path = dir.join("nested").join("coqod.log");
        let config = LogConfig {
            default_directive: "info".to_owned(),
            format: LogFormat::Json,
            target: LogTarget::File(path.clone()),
        };
        init(&config).unwrap();
        tracing::info!(answer = 42, "hello from the kernel test");
        let contents = fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("hello from the kernel test"),
            "{contents}"
        );
        assert!(contents.contains("\"answer\":42"), "{contents}");
        assert!(matches!(
            init(&config),
            Err(LogInitError::AlreadyInitialised)
        ));
        let _ = fs::remove_dir_all(dir);
    }
}
