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

fn init_logging() {
    let level = save_guard::paths::config_path().map_or_else(
        |_| "info".to_string(),
        |p| save_guard::config::load(&p).config.log_level,
    );
    if let Ok(dir) = save_guard::paths::log_dir() {
        let _ = save_guard::logging::init(&dir, &level);
    }
}
