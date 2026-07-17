//! The Steam-launched background monitor (`--monitor` mode).
//!
//! Hard invariants:
//! 1. **Never block the game.** Every backup step is best-effort; on any error
//!    we log and still launch/keep the game running.
//! 2. **Live exactly as long as the game.** Steam treats the game as running
//!    while this process lives, so we wait for `eldenring.exe` to appear and
//!    then disappear — not merely for our direct child (the EAC launcher) to
//!    exit, since that can die first.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::discovery::{self, SaveCandidate};
use crate::launch::{GameCommand, parse_monitor_args};
use crate::platform::{self, SingleInstance};
use crate::snapshot::{self, Reason};
use crate::{GAME_PROCESS, retention};

const MUTEX_NAME: &str = "EldenRingSaveGuardMonitor";
const APPEAR_TIMEOUT: Duration = Duration::from_secs(180);
const PROCESS_POLL: Duration = Duration::from_secs(2);
const POST_EXIT_GRACE: Duration = Duration::from_secs(8);

/// Entry point for `--monitor`. Returns the process exit code.
///
/// Note: with name-based process detection we cannot recover the game's real
/// exit code, so a normal session always returns 0 (documented limitation).
pub fn run<I, S>(args_after_program: I) -> i32
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let Some(cmd) = parse_monitor_args(args_after_program) else {
        tracing::error!("monitor invoked without a wrapped game command");
        return 1;
    };

    // Resolve config + save + destination. Failure here must NOT stop the game.
    let target = resolve_target();
    if let Err(e) = &target {
        tracing::warn!("backups disabled this session: {e:#}");
    }

    // Single-instance: if another monitor is already running we still launch
    // and track the game, but skip snapshotting to avoid duplicate work.
    let lock = SingleInstance::acquire(MUTEX_NAME);
    let monitoring = lock.is_some() && target.is_ok();
    if lock.is_none() {
        tracing::info!("another monitor is active; launching without a second monitor");
    }

    // Best-effort pre-launch snapshot.
    if monitoring
        && let Ok(t) = &target
        && t.config.pre_launch
    {
        backup(t, Reason::PreLaunch);
    }

    let mut child = match spawn_game(&cmd) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to launch the game: {e:#}");
            return 1;
        }
    };
    tracing::info!("game launcher started, waiting for {GAME_PROCESS}");

    if !monitoring {
        let _ = child.wait();
        return 0;
    }
    let target = target.expect("checked by `monitoring`");

    watch_session(&target, &mut child);
    tracing::info!("session ended");
    0
}

fn spawn_game(cmd: &GameCommand) -> std::io::Result<std::process::Child> {
    // Forward program + args verbatim; inherit cwd/env so EAC and Steam
    // tracking behave exactly as an unwrapped launch.
    Command::new(&cmd.program).args(&cmd.args).spawn()
}

struct Target {
    config: Config,
    elden_root: PathBuf,
    steamid: String,
    dest: PathBuf,
}

impl Target {
    fn candidate(&self) -> Option<SaveCandidate> {
        discovery::find(&self.elden_root, &self.steamid)
    }
}

fn resolve_target() -> anyhow::Result<Target> {
    use anyhow::{Context, bail};
    let config = crate::config::load(&crate::paths::config_path()?).config;
    let steamid = config
        .selected_steamid
        .clone()
        .context("no Steam account selected in configuration")?;
    let dest = config
        .backup_dest
        .clone()
        .context("no backup destination configured")?;
    let elden_root = crate::paths::elden_ring_root()?;
    let candidate = discovery::find(&elden_root, &steamid);
    if candidate.is_none() {
        bail!(
            "selected save {steamid} not found under {}",
            elden_root.display()
        );
    }
    crate::paths::validate_backup_dest(&candidate.expect("checked above").dir, &dest)?;
    Ok(Target {
        config,
        elden_root,
        steamid,
        dest,
    })
}

/// Poll for the game, take periodic snapshots on content change, then wait for
/// exit and take a final snapshot.
fn watch_session(target: &Target, child: &mut std::process::Child) {
    if !wait_for(|| platform::process_running(GAME_PROCESS), APPEAR_TIMEOUT) {
        tracing::warn!("{GAME_PROCESS} did not appear within timeout; waiting on launcher");
        let _ = child.wait();
        return;
    }
    tracing::info!("{GAME_PROCESS} running");

    let interval = Duration::from_secs(target.config.interval_secs);
    let mut last_periodic = Instant::now();
    while platform::process_running(GAME_PROCESS) {
        std::thread::sleep(PROCESS_POLL);
        if target.config.periodic && last_periodic.elapsed() >= interval {
            backup(target, Reason::Periodic);
            last_periodic = Instant::now();
        }
    }

    tracing::info!("{GAME_PROCESS} exited; waiting for final writes");
    std::thread::sleep(POST_EXIT_GRACE);
    let _ = child.try_wait();
    if target.config.post_exit {
        backup(target, Reason::PostExit);
    }
}

/// Best-effort snapshot + retention. Logs outcome; never propagates errors.
fn backup(target: &Target, reason: Reason) {
    let Some(candidate) = target.candidate() else {
        tracing::warn!(
            "save {} vanished; skipping {} backup",
            target.steamid,
            reason.label()
        );
        return;
    };
    let sources = source_files(&candidate);
    match snapshot::create(&target.dest, &target.steamid, &sources, reason) {
        Ok(Some(snap)) => {
            tracing::info!(
                "{} snapshot created: {}",
                reason.label(),
                snap.dir.display()
            );
            match retention::apply(&target.dest, &target.steamid, target.config.retention) {
                Ok(removed) if !removed.is_empty() => {
                    tracing::info!("retention removed {} old snapshot(s)", removed.len());
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("retention failed: {e:#}"),
            }
        }
        Ok(None) => tracing::debug!("{} backup skipped: save unchanged", reason.label()),
        Err(e) => tracing::warn!("{} backup failed: {e:#}", reason.label()),
    }
}

/// The vanilla save file plus its `.sl2.bak` sibling when present.
pub fn source_files(candidate: &SaveCandidate) -> Vec<PathBuf> {
    let mut v = vec![candidate.save_file.clone()];
    if let Some(bak) = &candidate.bak_file {
        v.push(bak.clone());
    }
    v
}

/// Poll `cond` every [`PROCESS_POLL`] until it is true or `timeout` elapses.
fn wait_for(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(PROCESS_POLL);
    }
    cond()
}

/// Convenience for the GUI: is a monitor currently running?
pub fn monitor_active() -> bool {
    // If we can acquire the lock, none was held; release immediately.
    match SingleInstance::acquire(MUTEX_NAME) {
        Some(_guard) => false,
        None => true,
    }
}
