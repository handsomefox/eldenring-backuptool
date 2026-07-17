//! File logging under the app data dir. Truncates at startup if oversized so
//! logs never grow without bound. No credentials or unrelated paths are logged.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use anyhow::Result;

const MAX_LOG_BYTES: u64 = 1_048_576;

enum Sink {
    File(File),
    Null(io::Sink),
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Sink::File(f) => f.write(buf),
            Sink::Null(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::File(f) => f.flush(),
            Sink::Null(s) => s.flush(),
        }
    }
}

/// Initialize the global tracing subscriber to write to `<log_dir>/app.log`.
/// Safe to call once; subsequent calls are no-ops.
pub fn init(log_dir: &Path, level: &str) -> Result<()> {
    std::fs::create_dir_all(log_dir)?;
    let path = log_dir.join("app.log");
    if std::fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = File::create(&path); // truncate
    }

    let path = path.clone();
    let make = move || match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => Sink::File(f),
        Err(_) => Sink::Null(io::sink()),
    };

    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(make)
        .try_init();
    Ok(())
}
