#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Entry point. Normal launch opens the GUI; `--monitor <game command>`
//! (from Steam launch options) runs the background monitor.

mod gui;

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let monitor_mode = args
        .first()
        .and_then(|a| a.to_str())
        .is_some_and(|a| a == save_guard::launch::MONITOR_FLAG);

    init_logging();

    if monitor_mode {
        let code = save_guard::monitor::run(args);
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }

    match gui::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("GUI exited with error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Logs both modes to daily files in the logs folder, at the level the
/// settings file names unless `RUST_LOG` is set.
fn init_logging() {
    let level = save_guard::paths::config_path().map_or_else(
        |_| "info".to_string(),
        |p| save_guard::config::log_level(&p),
    );
    let dir = save_guard::paths::log_dir().ok();
    app_diagnostics::init(dir.as_deref(), "eldenring-backuptool", &level);
}
