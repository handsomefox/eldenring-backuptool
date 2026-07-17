//! Elden Ring Save Guard core: portable, GUI-free logic.
//!
//! Nothing in this crate references eframe/egui/rfd, so
//! `cargo test --lib --no-default-features` builds and runs on any platform.

pub mod config;
pub mod discovery;
pub mod launch;
pub mod logging;
pub mod monitor;
pub mod paths;
pub mod platform;
pub mod retention;
pub mod snapshot;

pub const APP_NAME: &str = "EldenRingSaveGuard";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Name of the running Elden Ring process the monitor waits on.
pub const GAME_PROCESS: &str = "eldenring.exe";
