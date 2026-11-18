//! The Elden Ring Save Guard desktop app: a compact dashboard for choosing the
//! save to protect, checking status, browsing and restoring snapshots, and
//! copying the Steam launch option.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Utc};

use save_guard::config::{self, Config, MAX_RETENTION, MIN_INTERVAL_SECS};
use save_guard::discovery::{self, SaveCandidate};
use save_guard::launch;
use save_guard::paths;
use save_guard::platform;
use save_guard::restore;
use save_guard::retention::{self, KEEP_PER_SESSION_STAGE};
use save_guard::snapshot::{self, Checked, Reason, Snapshot};
use save_guard::update::{self, Updater};

/// Where Save Guard's releases are published.
const APP_INFO: update::AppInfo = update::AppInfo {
    name: "Save Guard",
    owner: "handsomefox",
    repo: "eldenring-backuptool",
    bin: "eldenring-backuptool",
    version: save_guard::APP_VERSION,
};

/// How long the dashboard reuses its last check for a running game. Each
/// check enumerates every process, which is too slow to repeat every frame.
const GAME_CHECK_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([560.0, 420.0])
            .with_title("Elden Ring Save Guard")
            // Without an icon eframe shows egui's own in the title bar and
            // taskbar.
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
                    .expect("the bundled app icon is a valid PNG"),
            ),
        ..Default::default()
    };
    // The guard lives only for the window, so an updated copy started below
    // finds the lock free.
    let result = {
        // A second launch brings this window forward instead of opening
        // another. The monitor never gets here: it has its own lock.
        let instance = match app_instance::acquire(APP_INFO.bin) {
            Ok(app_instance::Instance::Primary(guard)) => Some(guard),
            Ok(app_instance::Instance::Secondary) => {
                tracing::info!("another copy is running; asked it to come forward");
                return Ok(());
            }
            // Run without the guard rather than refuse to start.
            Err(error) => {
                tracing::warn!(%error, "running without the single-instance lock");
                None
            }
        };
        eframe::run_native(
            "Elden Ring Save Guard",
            options,
            Box::new(|cc| {
                if let Some(guard) = &instance {
                    bring_forward_on_activate(guard, &cc.egui_ctx);
                }
                Ok(Box::new(App::new(cc.egui_ctx.clone())))
            }),
        )
    };
    // Only the dashboard updates. The window has closed by now, so the new
    // copy starts after this one has written everything it meant to.
    if let Err(error) = update::restart_if_requested() {
        tracing::error!("could not start the updated app: {error}");
    }
    result
}

/// Restores and focuses the window whenever another launch signals.
fn bring_forward_on_activate(guard: &app_instance::Guard, ctx: &egui::Context) {
    let ctx = ctx.clone();
    let watching = guard.on_activate(move || {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    });
    if let Err(error) = watching {
        tracing::warn!(%error, "a second launch will not bring this window forward");
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Dashboard,
    Backups,
    Settings,
    Help,
}

/// Which backup stages the Settings tab shows checked.
struct Stages {
    pre_launch: bool,
    periodic: bool,
    post_exit: bool,
}

struct App {
    config: Config,
    config_path: Option<PathBuf>,
    elden_root: Option<PathBuf>,
    candidates: Vec<SaveCandidate>,
    /// The selected account's snapshots, newest first, damaged ones included.
    snapshots: Vec<Checked>,
    tab: Tab,
    status: Option<(bool, String)>, // (is_error, message)
    recovered: Option<PathBuf>,
    // Settings editors
    dest_edit: String,
    interval_edit: String,
    retention_edit: String,
    /// The stage toggles as edited, committed to `config` only by Apply.
    stages_edit: Stages,
    ctx: egui::Context,
    job: Option<Job>,
    /// A refresh asked for while a job ran, started once it finishes.
    pending_refresh: bool,
    /// The snapshot the user chose to restore, waiting for confirmation.
    restore_choice: Option<RestoreChoice>,
    free_space: Option<u64>,
    updater: Updater,
    update_window_open: bool,
    /// When the game was last looked for, and whether it was running.
    game_check: Option<(Instant, bool)>,
    /// The Steam launch option for this executable, shown on the Help tab.
    launch_option: String,
}

/// Background work in flight. Only one job runs at a time.
struct Job {
    kind: JobKind,
    /// The account and destination the job loads snapshots for. A result for
    /// any other pair is stale and is not shown.
    inputs: ViewInputs,
    receiver: Receiver<LoadedView>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobKind {
    Refresh,
    Backup,
    Restore,
}

/// What a loaded view depends on.
#[derive(Clone, PartialEq, Eq)]
struct ViewInputs {
    steamid: Option<String>,
    dest: Option<PathBuf>,
}

impl ViewInputs {
    fn of(config: &Config) -> Self {
        Self {
            steamid: config.selected_steamid.clone(),
            dest: config.backup_dest.clone(),
        }
    }
}

/// A snapshot the user picked to restore, with the account and destination
/// it was picked under.
struct RestoreChoice {
    snapshot: Snapshot,
    inputs: ViewInputs,
}

struct LoadedView {
    candidates: Vec<SaveCandidate>,
    snapshots: Vec<Checked>,
    free_space: Option<u64>,
    status: Option<(bool, String)>,
}

fn load_view(config: &Config, root: Option<&Path>) -> LoadedView {
    let candidates = root.map_or_else(Vec::new, discovery::discover);
    let mut snapshots = match (&config.selected_steamid, &config.backup_dest) {
        (Some(id), Some(dest)) => snapshot::list_all(dest, id),
        _ => Vec::new(),
    };
    snapshots.reverse();
    LoadedView {
        candidates,
        snapshots,
        free_space: config.backup_dest.as_deref().and_then(platform::free_space),
        status: None,
    }
}

fn spawn_job<T: Send + 'static>(
    ctx: egui::Context,
    work: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<Receiver<T>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("save-guard-worker".into())
        .spawn(move || {
            let _ = sender.send(work());
            ctx.request_repaint();
        })?;
    Ok(receiver)
}

impl App {
    fn new(ctx: egui::Context) -> Self {
        let config_path = paths::config_path().ok();
        let (config, recovered) = match &config_path {
            Some(p) => {
                let r = config::load(p);
                (r.config, r.recovered_from)
            }
            None => (Config::default(), None),
        };
        let elden_root = paths::elden_ring_root().ok();
        let exe =
            std::env::current_exe().unwrap_or_else(|_| PathBuf::from("eldenring-backuptool.exe"));
        let mut app = Self {
            dest_edit: config
                .backup_dest
                .as_ref()
                .map_or_default(|p| p.display().to_string()),
            interval_edit: config.interval_secs.to_string(),
            retention_edit: config.retention.to_string(),
            stages_edit: Stages {
                pre_launch: config.pre_launch,
                periodic: config.periodic,
                post_exit: config.post_exit,
            },
            config,
            config_path,
            elden_root,
            candidates: Vec::new(),
            snapshots: Vec::new(),
            tab: Tab::Dashboard,
            status: None,
            recovered,
            updater: Updater::new(APP_INFO, {
                let ctx = ctx.clone();
                move || ctx.request_repaint()
            }),
            update_window_open: false,
            ctx,
            job: None,
            pending_refresh: false,
            restore_choice: None,
            free_space: None,
            game_check: None,
            launch_option: launch::launch_option(&exe),
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        let config = self.config.clone();
        let root = self.elden_root.clone();
        self.start_job(JobKind::Refresh, move || {
            load_view(&config, root.as_deref())
        });
    }

    /// Starts `work` on a worker thread. A refresh asked for during another
    /// job runs once that job finishes; any other job is refused with a
    /// message, never dropped silently.
    fn start_job(&mut self, kind: JobKind, work: impl FnOnce() -> LoadedView + Send + 'static) {
        if self.job.is_some() {
            if kind == JobKind::Refresh {
                self.pending_refresh = true;
            } else {
                self.status = Some((
                    true,
                    "Wait for the current task to finish, then try again.".into(),
                ));
            }
            return;
        }
        match spawn_job(self.ctx.clone(), work) {
            Ok(receiver) => {
                self.job = Some(Job {
                    kind,
                    inputs: ViewInputs::of(&self.config),
                    receiver,
                });
            }
            Err(error) => {
                self.status = Some((true, format!("Could not start background work: {error}")));
            }
        }
    }

    fn poll_job(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        match job.receiver.try_recv() {
            Ok(view) => {
                // A job's own message always counts, but a list loaded for
                // another account or destination is replaced, not shown.
                if job.inputs == ViewInputs::of(&self.config) {
                    self.candidates = view.candidates;
                    self.snapshots = view.snapshots;
                    self.free_space = view.free_space;
                } else {
                    self.pending_refresh = true;
                }
                if view.status.is_some() {
                    self.status = view.status;
                }
            }
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.status = Some((true, "Background work stopped unexpectedly.".into()));
            }
        }
        self.job = None;
        if std::mem::take(&mut self.pending_refresh) {
            self.refresh();
        }
    }

    /// The running job, if it writes files, so the window must not close.
    fn writing(&self) -> Option<JobKind> {
        self.job
            .as_ref()
            .map(|job| job.kind)
            .filter(|kind| *kind != JobKind::Refresh)
    }

    /// The snapshots that passed their check, newest first.
    fn good_snapshots(&self) -> impl Iterator<Item = &Snapshot> {
        self.snapshots
            .iter()
            .filter(|checked| !checked.is_damaged())
            .map(|checked| &checked.snapshot)
    }

    fn game_running(&mut self) -> bool {
        let now = Instant::now();
        match self.game_check {
            Some((checked, running)) if now.duration_since(checked) < GAME_CHECK_INTERVAL => {
                running
            }
            _ => {
                let running = platform::process_running(save_guard::GAME_PROCESS);
                self.game_check = Some((now, running));
                running
            }
        }
    }

    fn selected_candidate(&self) -> Option<&SaveCandidate> {
        let id = self.config.selected_steamid.as_ref()?;
        self.candidates.iter().find(|c| &c.steamid == id)
    }

    /// Writes the settings, and reports whether they were written. On
    /// failure the error is already in the status bar, and a success message
    /// must not replace it.
    fn save_config(&mut self) -> bool {
        let Some(path) = &self.config_path else {
            self.status = Some((
                true,
                "Could not save settings: the %LOCALAPPDATA% folder could not be found.".into(),
            ));
            return false;
        };
        match config::save(path, &self.config) {
            Ok(()) => true,
            Err(e) => {
                self.status = Some((true, format!("Could not save settings: {e}")));
                false
            }
        }
    }

    /// Writes only the update settings. The Settings tab edits `self.config`
    /// in place until Apply, so saving all of it here would also apply
    /// changes the user has not confirmed.
    fn save_update_settings(&mut self) {
        let Some(path) = &self.config_path else {
            return;
        };
        let mut on_disk = config::load(path).config;
        on_disk.app_updates = self.config.app_updates.clone();
        if let Err(e) = config::save(path, &on_disk) {
            self.status = Some((true, format!("Could not save update settings: {e}")));
        }
    }

    fn select_account(&mut self, steamid: &str) {
        let previous_default = self
            .config
            .selected_steamid
            .as_deref()
            .and_then(|id| paths::default_backup_dest(id).ok());
        let was_using_default = self.config.backup_dest.as_ref() == previous_default.as_ref();
        let previous = self.config.clone();
        self.config.selected_steamid = Some(steamid.to_string());
        if (self.config.backup_dest.is_none() || was_using_default)
            && let Ok(dest) = paths::default_backup_dest(steamid)
        {
            self.config.backup_dest = Some(dest.clone());
            self.dest_edit = dest.display().to_string();
        }
        // The background monitor reads the file, so the dashboard keeps what
        // the file holds when it cannot be written.
        let saved = self.save_config();
        if !saved {
            self.config = previous;
            self.dest_edit = self
                .config
                .backup_dest
                .as_ref()
                .map_or_default(|dest| dest.display().to_string());
        }
        self.refresh();
        if saved {
            self.status = Some((false, format!("Protecting account {steamid}")));
        }
    }

    fn backup_now(&mut self) {
        let Some(candidate) = self.selected_candidate().cloned() else {
            self.status = Some((true, "Select a Steam account first.".into()));
            return;
        };
        let Some(dest) = self.config.backup_dest.clone() else {
            self.status = Some((true, "Choose a backup destination first.".into()));
            return;
        };
        let config = self.config.clone();
        let root = self.elden_root.clone();
        self.start_job(JobKind::Backup, move || {
            let result = (|| -> anyhow::Result<String> {
                // Found again now, and again once any other backup is done:
                // the game may have written or removed its .bak since the
                // list was last refreshed.
                let find = || match root.as_deref() {
                    Some(root) => discovery::find(root, &candidate.steamid).ok_or_else(|| {
                        anyhow::anyhow!("the save folder for {} is gone", candidate.steamid)
                    }),
                    None => Ok(candidate.clone()),
                };
                // Before the backup takes its lock, which is a file there.
                paths::validate_backup_dest(&find()?.dir, &dest)?;
                match retention::create_and_prune(
                    &dest,
                    &candidate.steamid,
                    || Ok(find()?.source_files()),
                    Reason::Manual,
                    config.retention,
                )? {
                    Some((snap, _)) => Ok(format!("Backed up: {}", dir_name(&snap.dir))),
                    None => Ok("Save unchanged. No new backup needed.".into()),
                }
            })();
            let mut view = load_view(&config, root.as_deref());
            view.status = Some(match result {
                Ok(message) => (false, message),
                Err(error) => (true, format!("Backup failed: {error}")),
            });
            view
        });
    }

    /// Asks to restore `snapshot`, after a confirmation.
    fn choose_restore(&mut self, snapshot: &Snapshot) {
        self.restore_choice = Some(RestoreChoice {
            snapshot: snapshot.clone(),
            inputs: ViewInputs::of(&self.config),
        });
    }

    /// Restores the confirmed snapshot on the job thread.
    fn restore_now(&mut self, choice: RestoreChoice) {
        if choice.inputs != ViewInputs::of(&self.config) {
            self.status = Some((
                true,
                "The account or backup folder changed. Choose the backup again.".into(),
            ));
            return;
        }
        let (Some(steamid), Some(dest)) = (choice.inputs.steamid, choice.inputs.dest) else {
            self.status = Some((true, "Select a Steam account first.".into()));
            return;
        };
        let Some(root) = self.elden_root.clone() else {
            self.status = Some((true, "Could not find the Elden Ring save folder.".into()));
            return;
        };
        // Checked now for a quick answer, and again on the job thread.
        self.game_check = None;
        if self.game_running() {
            self.status = Some((true, "Close Elden Ring, then restore.".into()));
            return;
        }
        let config = self.config.clone();
        let when = local_time(choice.snapshot.metadata.created_utc);
        self.start_job(JobKind::Restore, move || {
            let request = restore::Request {
                dest: &dest,
                steamid: &steamid,
                snapshot_dir: &choice.snapshot.dir,
                elden_root: &root,
            };
            let result = restore::restore(&request, || {
                platform::process_running(save_guard::GAME_PROCESS)
            });
            let mut view = load_view(&config, Some(&root));
            view.status = Some(match result {
                Ok(restored) => (false, restored_message(&when, &restored)),
                Err(error) => (true, format!("Restore failed: {error:#}")),
            });
            view
        });
    }

    fn apply_settings(&mut self) {
        let Ok(interval) = self.interval_edit.trim().parse::<u64>() else {
            self.status = Some((true, "Interval must be a whole number of seconds.".into()));
            return;
        };
        let Ok(retention) = self.retention_edit.trim().parse::<usize>() else {
            self.status = Some((true, "Retention must be a whole number.".into()));
            return;
        };
        if interval < MIN_INTERVAL_SECS {
            self.status = Some((
                true,
                format!("Interval must be at least {MIN_INTERVAL_SECS}s."),
            ));
            return;
        }
        if retention == 0 || retention > MAX_RETENTION {
            self.status = Some((
                true,
                format!("Retention must be between 1 and {MAX_RETENTION}."),
            ));
            return;
        }
        let dest = PathBuf::from(self.dest_edit.trim());
        if dest.as_os_str().is_empty() {
            self.status = Some((true, "Backup destination cannot be empty.".into()));
            return;
        }
        if let Some(c) = self.selected_candidate().cloned()
            && let Err(e) = paths::validate_backup_dest(&c.dir, &dest)
        {
            self.status = Some((true, e.to_string()));
            return;
        }
        let previous = self.config.clone();
        self.config.interval_secs = interval;
        self.config.retention = retention;
        self.config.backup_dest = Some(dest);
        self.config.pre_launch = self.stages_edit.pre_launch;
        self.config.periodic = self.stages_edit.periodic;
        self.config.post_exit = self.stages_edit.post_exit;
        let saved = self.save_config();
        if !saved {
            self.config = previous;
        }
        self.refresh();
        if saved {
            self.status = Some((false, "Settings saved.".into()));
        }
    }
}

impl eframe::App for App {
    /// Runs before every frame, and also while the window is minimized, when eframe calls only
    /// this. A close request mid-backup then still waits for the backup.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_job();
        if let Some(kind) = self.writing()
            && ctx.input(|input| input.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            // A close from the taskbar can come while minimized; bring the
            // window back so the reason is seen.
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.status = Some((
                false,
                if kind == JobKind::Restore {
                    "Wait for the restore to finish before closing."
                } else {
                    "Wait for the backup to finish before closing."
                }
                .into(),
            ));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let before = self.config.app_updates.clone();
        self.updater.poll(&mut self.config.app_updates);
        if self.config.app_updates != before {
            self.save_update_settings();
        }
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("🛡 Elden Ring Save Guard");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Dashboard, "Dashboard");
                ui.selectable_value(&mut self.tab, Tab::Backups, "Backups");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                ui.selectable_value(&mut self.tab, Tab::Help, "Help");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(self.job.is_none(), egui::Button::new("⟳ Refresh"))
                        .clicked()
                    {
                        self.refresh();
                    }
                    self.update_chip(ui);
                });
            });
        });

        if let Some((is_err, msg)) = &self.status {
            let color = if *is_err { RED } else { GREEN };
            let prefix = if *is_err { "Error" } else { "OK" };
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(color, format!("{prefix}:"));
                    ui.label(msg);
                });
            });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            if self.job.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(match self.writing() {
                        Some(JobKind::Backup) => "Creating backup…",
                        Some(JobKind::Restore) => "Restoring backup…",
                        _ => "Loading saves and backups…",
                    });
                });
            }
            ui.add_enabled_ui(self.job.is_none(), |ui| match self.tab {
                Tab::Dashboard => self.dashboard(ui),
                Tab::Backups => self.backups(ui),
                Tab::Settings => self.settings(ui),
                Tab::Help => self.help(ui),
            });
        });

        if self.restore_choice.is_some() {
            self.restore_dialog(ui.ctx());
        }
        if self.update_window_open {
            self.update_window(ui.ctx());
        }
    }
}

impl App {
    #[expect(
        clippy::too_many_lines,
        reason = "the dashboard is a single immediate-mode UI composition without reusable business logic"
    )]
    fn dashboard(&mut self, ui: &mut egui::Ui) {
        if let Some(bad) = &self.recovered {
            ui.colored_label(
                AMBER,
                format!(
                    "Your settings file was unreadable and was reset. The old file is kept at {}",
                    bad.display()
                ),
            );
            ui.separator();
        }

        if self.elden_root.is_none() {
            ui.label("Could not resolve %APPDATA%\\EldenRing. Run this on the PC where Elden Ring is installed.");
            return;
        }
        if self.candidates.is_empty() {
            ui.label("No Elden Ring saves found under %APPDATA%\\EldenRing yet. Launch the game once to create a save, then click Refresh.");
            return;
        }

        // Protection status banner.
        let good = self.good_snapshots().count();
        let protected =
            self.config.selected_steamid.is_some() && self.config.backup_dest.is_some() && good > 0;
        let (color, text) = if protected {
            (GREEN, "Protected — backups exist")
        } else if self.config.selected_steamid.is_none() {
            (AMBER, "Setup incomplete — choose an account below")
        } else if self.snapshots.is_empty() {
            (
                AMBER,
                "No backups yet — launch the game or press Back up now",
            )
        } else if good == 0 {
            (RED, "Every stored backup is damaged — press Back up now")
        } else {
            (AMBER, "Setup incomplete")
        };
        let game_running = self.game_running();
        ui.horizontal(|ui| {
            // Painted rather than typed: egui's default fonts have no "●".
            let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.5, color);
            ui.strong(text);
            if game_running {
                ui.separator();
                ui.label("Elden Ring is running");
            }
        });
        ui.separator();

        egui::Grid::new("status_grid")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                let sel = self
                    .config
                    .selected_steamid
                    .clone()
                    .unwrap_or_else(|| "(none)".into());
                ui.label("Selected account:");
                ui.label(sel);
                ui.end_row();

                if let Some(c) = self.selected_candidate() {
                    ui.label("Save file:");
                    ui.label(dir_name(&c.save_file));
                    ui.end_row();
                    ui.label("Save modified:");
                    ui.label(c.modified.map_or_else(|| "unknown".into(), system_local));
                    ui.end_row();
                    ui.label("Save size:");
                    ui.label(human_size(c.size));
                    ui.end_row();
                }

                ui.label("Backup destination:");
                ui.label(
                    self.config
                        .backup_dest
                        .as_ref()
                        .map_or_else(|| "(not set)".into(), |p| p.display().to_string()),
                );
                ui.end_row();

                if let Some(free) = self.free_space {
                    ui.label("Free space:");
                    ui.label(human_size(free));
                    ui.end_row();
                }

                ui.label("Snapshots stored:");
                let damaged = self.snapshots.len() - good;
                if damaged == 0 {
                    ui.label(good.to_string());
                } else {
                    ui.colored_label(RED, format!("{good} ({damaged} more damaged)"));
                }
                ui.end_row();

                ui.label("Storage used:");
                let used: u64 = self.good_snapshots().map(Snapshot::stored_size).sum();
                let original: u64 = self.good_snapshots().map(Snapshot::original_size).sum();
                ui.label(if original > 0 {
                    format!(
                        "{} (compressed from {})",
                        human_size(used),
                        human_size(original)
                    )
                } else {
                    human_size(used)
                });
                ui.end_row();

                ui.label("Last backup:");
                let last = self.good_snapshots().next().map_or_else(
                    || "never".into(),
                    |s| {
                        format!(
                            "{} ({})",
                            local_time(s.metadata.created_utc),
                            relative_age(s.metadata.created_utc)
                        )
                    },
                );
                ui.label(last);
                ui.end_row();

                ui.label("Steam integration:");
                ui.label("paste the launch option (see Help tab)");
                ui.end_row();
            });

        ui.separator();
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.job.is_none(), egui::Button::new("💾 Back up now"))
                .clicked()
            {
                self.backup_now();
            }
        });

        ui.add_space(8.0);
        ui.strong("Steam accounts with saves");
        ui.label("Only the account you select here is backed up. Switching does not delete another account's backups.");
        let selected = self.config.selected_steamid.as_deref();
        let mut clicked = None;
        for c in &self.candidates {
            let is_sel = selected == Some(c.steamid.as_str());
            let label = format!(
                "{}  —  saved {}",
                c.steamid,
                c.modified.map_or_else(|| "unknown".into(), system_local)
            );
            if ui.radio(is_sel, label).clicked() && !is_sel {
                clicked = Some(c.steamid.clone());
            }
        }
        if let Some(steamid) = clicked {
            self.select_account(&steamid);
        }
    }

    fn backups(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.job.is_none(), egui::Button::new("💾 Back up now"))
                .clicked()
            {
                self.backup_now();
            }
            if let Some(dest) = &self.config.backup_dest
                && ui.button("📂 Open backup folder").clicked()
            {
                open_folder(&snapshot::snapshots_dir(dest));
            }
        });
        ui.separator();

        if self.snapshots.is_empty() {
            ui.label("No snapshots yet for the selected account.");
            return;
        }

        let game_running = self.game_running();
        let mut chosen = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("snap_grid")
                .num_columns(6)
                .striped(true)
                .spacing([14.0, 4.0])
                .show(ui, |ui| {
                    ui.strong("When");
                    ui.strong("Age");
                    ui.strong("Size");
                    ui.strong("Reason");
                    ui.strong("");
                    ui.strong("");
                    ui.end_row();
                    for checked in &self.snapshots {
                        let s = &checked.snapshot;
                        ui.label(local_time(s.metadata.created_utc));
                        ui.label(relative_age(s.metadata.created_utc));
                        ui.label(human_size(s.stored_size()));
                        if let Some(damage) = &checked.damage {
                            ui.colored_label(RED, "damaged").on_hover_text(format!(
                                "This backup failed its check and cannot be restored: {damage}"
                            ));
                        } else {
                            ui.label(s.metadata.reason.label());
                        }
                        if ui.button("Open").clicked() {
                            open_folder(&s.dir);
                        }
                        let restore = ui.add_enabled(
                            !checked.is_damaged() && !game_running && self.job.is_none(),
                            egui::Button::new("Restore…"),
                        );
                        let restore = if checked.is_damaged() {
                            restore.on_disabled_hover_text("A damaged backup cannot be restored.")
                        } else if game_running {
                            restore.on_disabled_hover_text("Close Elden Ring to restore.")
                        } else {
                            restore
                                .on_hover_text("Put this backup back as your save, after backing up the current one")
                        };
                        if restore.clicked() {
                            chosen = Some(s.clone());
                        }
                        ui.end_row();
                    }
                });
        });
        if let Some(snapshot) = chosen {
            self.choose_restore(&snapshot);
        }
    }

    /// Asks the user to confirm the restore they chose.
    fn restore_dialog(&mut self, ctx: &egui::Context) {
        let Some(choice) = &self.restore_choice else {
            return;
        };
        let when = local_time(choice.snapshot.metadata.created_utc);
        let account = choice.inputs.steamid.clone().unwrap_or_default();
        let busy = self.job.is_some();
        let mut confirmed = false;
        let mut cancelled = false;
        let response = egui::Modal::new(egui::Id::unique("restore_confirm")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading("Restore this backup?");
            ui.add_space(4.0);
            ui.label(format!(
                "Replace the save of account {account} with the backup from {when}."
            ));
            ui.label(
                "Save Guard first backs up your current save as a new manual backup. \
                 If that backup fails, nothing is restored.",
            );
            ui.label(
                "Close Elden Ring first. When Steam next starts the game, if it reports a \
                 Steam Cloud conflict, choose the local copy, which is the one restored here.",
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy, egui::Button::new("Restore"))
                    .clicked()
                {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
        });
        if confirmed {
            if let Some(choice) = self.restore_choice.take() {
                self.restore_now(choice);
            }
        } else if cancelled || response.should_close() {
            self.restore_choice = None;
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("settings_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Backup destination:");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.dest_edit);
                    if ui.button("Browse…").clicked()
                        && let Some(dir) = rfd::FileDialog::new().pick_folder()
                    {
                        self.dest_edit = dir.display().to_string();
                    }
                });
                ui.end_row();

                ui.label("Backup interval (seconds):");
                ui.text_edit_singleline(&mut self.interval_edit);
                ui.end_row();

                ui.label("Keep newest snapshots:");
                ui.text_edit_singleline(&mut self.retention_edit);
                ui.end_row();
            });
        ui.label(format!(
            "Save Guard also keeps the newest {KEEP_PER_SESSION_STAGE} snapshots taken before the \
             game launched and the newest {KEEP_PER_SESSION_STAGE} taken after it exited."
        ));

        ui.add_space(4.0);
        ui.checkbox(
            &mut self.stages_edit.pre_launch,
            "Snapshot before the game launches",
        );
        ui.checkbox(
            &mut self.stages_edit.periodic,
            "Snapshot periodically while playing",
        );
        ui.checkbox(
            &mut self.stages_edit.post_exit,
            "Snapshot after the game exits",
        );

        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                self.apply_settings();
            }
            if ui.button("Open logs folder").clicked()
                && let Ok(dir) = paths::log_dir()
            {
                open_folder(&dir);
            }
        });
        ui.label(format!(
            "Intervals below {MIN_INTERVAL_SECS}s are rejected. Stage toggles apply the next time the game launches."
        ));
    }

    fn help(&mut self, ui: &mut egui::Ui) {
        ui.strong("Steam launch option");
        ui.label("Copy the command below into Steam > Elden Ring > Properties > General > Launch Options. This makes backups happen automatically every time you play, even with this window closed.");
        ui.add_space(4.0);

        let mut cmd = self.launch_option.clone();
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut cmd)
                    .desired_width(f32::INFINITY)
                    .interactive(true),
            );
        });
        if ui.button("📋 Copy launch option").clicked() {
            ui.ctx().copy_text(cmd.clone());
            self.status = Some((false, "Launch option copied to clipboard.".into()));
        }

        ui.separator();
        ui.strong("How to restore a save");
        ui.label("1. Fully close Elden Ring.");
        ui.label("2. Open the Backups tab, pick a snapshot, and click Restore… to put it back. Save Guard backs up your current save first.");
        ui.label("3. Start the game from Steam. If it reports a Steam Cloud conflict, choose the LOCAL copy — not the newer cloud one.");
        ui.label("To restore by hand instead, click Open, double-click save.zip, and copy the .sl2 (and .sl2.bak) into your save folder with Steam closed.");
        if ui.button("📂 Open my save folder").clicked() {
            if let Some(c) = self.selected_candidate() {
                open_folder(&c.dir);
            } else if let Some(root) = &self.elden_root {
                open_folder(root);
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!(
                "Elden Ring Save Guard v{}",
                save_guard::APP_VERSION
            ));
            if ui
                .button("Check for updates")
                .on_hover_text("Check GitHub for a new version now, and choose how often to check")
                .clicked()
            {
                // The label promises a check, so start one as the window
                // opens. The updater leaves a running check or download alone.
                self.updater.check_now();
                self.update_window_open = true;
            }
        });
        ui.label("Does not modify the game, inject code, or touch Easy Anti-Cheat. It only copies your save files.");
    }
}

const GREEN: egui::Color32 = egui::Color32::from_rgb(80, 160, 100);
const AMBER: egui::Color32 = egui::Color32::from_rgb(200, 140, 60);
const RED: egui::Color32 = egui::Color32::from_rgb(200, 80, 80);

/// Save Guard's colours and egui's own text styles, for the shared update
/// dialog.
struct UpdateLook;

impl app_update_ui::Look for UpdateLook {
    fn success(&self, _visuals: &egui::Visuals) -> egui::Color32 {
        GREEN
    }

    fn warning(&self, _visuals: &egui::Visuals) -> egui::Color32 {
        AMBER
    }
}

impl App {
    /// A small button in the header while a new version is out.
    fn update_chip(&mut self, ui: &mut egui::Ui) {
        let Some(text) = app_update_ui::chip_text(&self.updater, &self.config.app_updates) else {
            return;
        };
        let chip = egui::Button::new(egui::RichText::new(text).color(egui::Color32::WHITE))
            .fill(GREEN.gamma_multiply(0.8))
            .corner_radius(egui::CornerRadius::same(10));
        if ui
            .add(chip)
            .on_hover_text(app_update_ui::chip_hover_text(&self.updater))
            .clicked()
        {
            self.update_window_open = true;
        }
    }

    fn update_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut chosen = None;
        // A restart closes the window, so it waits for a backup, a restore,
        // or a reload.
        let busy = self
            .job
            .is_some()
            .then_some("Wait for the task in progress to finish");
        let before = self.config.app_updates.clone();
        let updater = &self.updater;
        let settings = &mut self.config.app_updates;
        egui::Window::new(app_update_ui::TITLE)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_width(480.0);
                chosen = app_update_ui::body(ui, &UpdateLook, updater, settings);
                ui.weak("The background monitor that Steam starts never checks.");
                chosen = app_update_ui::actions(ui, &UpdateLook, updater, busy).or(chosen);
            });
        if let Some(action) = chosen
            && app_update_ui::apply(action, ctx, &mut self.updater, &mut self.config.app_updates)
        {
            open = false;
        }
        // The period and a skipped version are saved as soon as they change.
        if self.config.app_updates.period != before.period
            || self.config.app_updates.skipped != before.skipped
        {
            self.save_update_settings();
        }
        self.update_window_open = open;
    }
}

/// Opens a folder in the system file manager. Best-effort: a failure is only
/// logged.
fn open_folder(path: &Path) {
    if let Err(error) = app_folders::open(path) {
        tracing::warn!(path = %path.display(), %error, "could not open the folder");
    }
}

/// The status line for a finished restore.
fn restored_message(when: &str, restored: &restore::Restored) -> String {
    let replaced = match &restored.backup {
        Some(backup) => format!(
            "The save it replaced is backed up as {}.",
            dir_name(&backup.dir)
        ),
        None => "The save it replaced matched the newest backup already.".to_string(),
    };
    let kept = restored.kept_aside.as_ref().map_or_default(|aside| {
        format!(
            " The replaced files changed during the restore and are kept in {}.",
            aside.display()
        )
    });
    format!("Restored the backup from {when}. {replaced}{kept}")
}

fn dir_name(p: &Path) -> String {
    p.file_name().map_or_else(
        || p.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

#[expect(
    clippy::cast_precision_loss,
    reason = "human-readable byte sizes are deliberately approximate"
)]
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn local_time(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn system_local(t: std::time::SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    local_time(dt)
}

fn relative_age(t: DateTime<Utc>) -> String {
    let secs = (Utc::now() - t).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_for_another_account_or_destination_is_stale() {
        let config = Config {
            selected_steamid: Some("111".into()),
            backup_dest: Some(PathBuf::from("/backups/111")),
            ..Config::default()
        };
        let inputs = ViewInputs::of(&config);
        assert!(inputs == ViewInputs::of(&config.clone()));
        let other_account = Config {
            selected_steamid: Some("222".into()),
            ..config.clone()
        };
        assert!(inputs != ViewInputs::of(&other_account));
        let other_dest = Config {
            backup_dest: Some(PathBuf::from("/elsewhere")),
            ..config.clone()
        };
        assert!(inputs != ViewInputs::of(&other_dest));
        // Settings that load no different list leave it current.
        let other_interval = Config {
            interval_secs: 900,
            ..config
        };
        assert!(inputs == ViewInputs::of(&other_interval));
    }

    #[test]
    fn background_job_does_not_block_the_caller() {
        let (release, wait) = mpsc::channel();
        let job = spawn_job(egui::Context::default(), move || {
            wait.recv().unwrap();
            42
        })
        .unwrap();
        assert!(matches!(job.try_recv(), Err(TryRecvError::Empty)));
        release.send(()).unwrap();
        assert_eq!(job.recv_timeout(Duration::from_secs(5)).unwrap(), 42);
    }
}
