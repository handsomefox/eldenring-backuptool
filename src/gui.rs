//! The Elden Ring Save Guard desktop app: a compact dashboard for choosing the
//! save to protect, checking status, browsing snapshots, and copying the Steam
//! launch option. Restore is intentionally manual (see README).

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use chrono::{DateTime, Local, Utc};

use save_guard::config::{self, Config, MAX_RETENTION, MIN_INTERVAL_SECS};
use save_guard::discovery::{self, SaveCandidate};
use save_guard::launch;
use save_guard::monitor::source_files;
use save_guard::paths;
use save_guard::platform;
use save_guard::snapshot::{self, Reason, Snapshot};
use save_guard::update::{self, CheckPeriod, NoteBlock, Updater};

/// Where Save Guard's releases are published.
const APP_INFO: update::AppInfo = update::AppInfo {
    owner: "handsomefox",
    repo: "eldenring-backuptool",
    bin: "eldenring-backuptool",
    version: save_guard::APP_VERSION,
};

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
    let result = eframe::run_native(
        "Elden Ring Save Guard",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc.egui_ctx.clone())))),
    );
    // Only the dashboard updates. The window has closed by now, so the new
    // copy starts after this one has written everything it meant to.
    if let Err(error) = update::restart_if_requested() {
        tracing::error!("could not start the updated app: {error}");
    }
    result
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Dashboard,
    Backups,
    Settings,
    Help,
}

struct App {
    config: Config,
    config_path: Option<PathBuf>,
    elden_root: Option<PathBuf>,
    candidates: Vec<SaveCandidate>,
    snapshots: Vec<Snapshot>,
    tab: Tab,
    status: Option<(bool, String)>, // (is_error, message)
    recovered: Option<PathBuf>,
    // Settings editors
    dest_edit: String,
    interval_edit: String,
    retention_edit: String,
    ctx: egui::Context,
    pending: Option<Receiver<LoadedView>>,
    backup_active: bool,
    free_space: Option<u64>,
    updater: Updater,
    update_window_open: bool,
}

/// What the user chose in the update window this frame.
enum UpdateAction {
    Download,
    Restart,
    Skip,
    Later,
}

struct LoadedView {
    candidates: Vec<SaveCandidate>,
    snapshots: Vec<Snapshot>,
    free_space: Option<u64>,
    status: Option<(bool, String)>,
}

fn load_view(config: &Config, root: Option<&std::path::Path>) -> LoadedView {
    let candidates = root.map_or_else(Vec::new, discovery::discover);
    let mut snapshots = match (&config.selected_steamid, &config.backup_dest) {
        (Some(id), Some(dest)) => snapshot::list(dest, id),
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
        let mut app = Self {
            dest_edit: config
                .backup_dest
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            interval_edit: config.interval_secs.to_string(),
            retention_edit: config.retention.to_string(),
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
            pending: None,
            backup_active: false,
            free_space: None,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        let config = self.config.clone();
        let root = self.elden_root.clone();
        self.start_job(false, move || load_view(&config, root.as_deref()));
    }

    fn reload_snapshots(&mut self) {
        self.refresh();
    }

    fn start_job(&mut self, backup: bool, work: impl FnOnce() -> LoadedView + Send + 'static) {
        if self.pending.is_some() {
            return;
        }
        match spawn_job(self.ctx.clone(), work) {
            Ok(receiver) => {
                self.pending = Some(receiver);
                self.backup_active = backup;
            }
            Err(error) => {
                self.status = Some((true, format!("Could not start background work: {error}")));
            }
        }
    }

    fn poll_job(&mut self) {
        let Some(receiver) = &self.pending else {
            return;
        };
        match receiver.try_recv() {
            Ok(view) => {
                self.candidates = view.candidates;
                self.snapshots = view.snapshots;
                self.free_space = view.free_space;
                if view.status.is_some() {
                    self.status = view.status;
                }
            }
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.status = Some((true, "Background work stopped unexpectedly.".into()));
            }
        }
        self.pending = None;
        self.backup_active = false;
    }

    fn selected_candidate(&self) -> Option<&SaveCandidate> {
        let id = self.config.selected_steamid.as_ref()?;
        self.candidates.iter().find(|c| &c.steamid == id)
    }

    fn save_config(&mut self) {
        if let Some(path) = &self.config_path {
            match config::save(path, &self.config) {
                Ok(()) => {}
                Err(e) => self.status = Some((true, format!("Could not save settings: {e}"))),
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
        self.config.selected_steamid = Some(steamid.to_string());
        if (self.config.backup_dest.is_none() || was_using_default)
            && let Ok(dest) = paths::default_backup_dest(steamid)
        {
            self.config.backup_dest = Some(dest.clone());
            self.dest_edit = dest.display().to_string();
        }
        self.save_config();
        self.reload_snapshots();
        self.status = Some((false, format!("Protecting account {steamid}")));
    }

    fn backup_now(&mut self) {
        if self.pending.is_some() {
            return;
        }
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
        self.start_job(true, move || {
            let result = (|| -> anyhow::Result<String> {
                paths::validate_backup_dest(&candidate.dir, &dest)?;
                let sources = source_files(&candidate);
                match snapshot::create(&dest, &candidate.steamid, &sources, Reason::Manual)? {
                    Some(snap) => {
                        save_guard::retention::apply(&dest, &candidate.steamid, config.retention)?;
                        Ok(format!("Backed up: {}", dir_name(&snap.dir)))
                    }
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
        self.config.interval_secs = interval;
        self.config.retention = retention;
        self.config.backup_dest = Some(dest);
        self.save_config();
        self.reload_snapshots();
        self.status = Some((false, "Settings saved.".into()));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_job();
        let before = self.config.app_updates.clone();
        self.updater.poll(&mut self.config.app_updates);
        if self.config.app_updates != before {
            self.save_update_settings();
        }
        if self.backup_active && ui.ctx().input(|input| input.viewport().close_requested()) {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.status = Some((
                false,
                "Wait for the backup to finish before closing.".into(),
            ));
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
                        .add_enabled(self.pending.is_none(), egui::Button::new("⟳ Refresh"))
                        .clicked()
                    {
                        self.refresh();
                    }
                    self.update_chip(ui);
                });
            });
        });

        if let Some((is_err, msg)) = &self.status {
            let color = if *is_err {
                egui::Color32::from_rgb(200, 80, 80)
            } else {
                egui::Color32::from_rgb(80, 160, 100)
            };
            let prefix = if *is_err { "Error" } else { "OK" };
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(color, format!("{prefix}:"));
                    ui.label(msg);
                });
            });
        }

        egui::CentralPanel::default().show(ui, |ui| {
            if self.pending.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(if self.backup_active {
                        "Creating backup…"
                    } else {
                        "Loading saves and backups…"
                    });
                });
            }
            ui.add_enabled_ui(self.pending.is_none(), |ui| match self.tab {
                Tab::Dashboard => self.dashboard(ui),
                Tab::Backups => self.backups(ui),
                Tab::Settings => self.settings(ui),
                Tab::Help => self.help(ui),
            });
        });

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
                egui::Color32::from_rgb(200, 140, 60),
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
        let protected = self.config.selected_steamid.is_some()
            && self.config.backup_dest.is_some()
            && !self.snapshots.is_empty();
        let (color, text) = if protected {
            (
                egui::Color32::from_rgb(80, 160, 100),
                "Protected — backups exist",
            )
        } else if self.config.selected_steamid.is_none() {
            (
                egui::Color32::from_rgb(200, 140, 60),
                "Setup incomplete — choose an account below",
            )
        } else if self.snapshots.is_empty() {
            (
                egui::Color32::from_rgb(200, 140, 60),
                "No backups yet — launch the game or press Back up now",
            )
        } else {
            (egui::Color32::from_rgb(200, 140, 60), "Setup incomplete")
        };
        ui.horizontal(|ui| {
            // Painted rather than typed: egui's default fonts have no "●".
            let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.5, color);
            ui.strong(text);
            if platform::process_running(save_guard::GAME_PROCESS) {
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
                ui.label(self.snapshots.len().to_string());
                ui.end_row();

                ui.label("Storage used:");
                let used: u64 = self.snapshots.iter().map(Snapshot::stored_size).sum();
                let original: u64 = self.snapshots.iter().map(Snapshot::original_size).sum();
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
                let last = self.snapshots.first().map_or_else(
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
            if ui.button("💾 Back up now").clicked() {
                self.backup_now();
            }
        });

        ui.add_space(8.0);
        ui.strong("Steam accounts with saves");
        ui.label("Only the account you select here is backed up. Switching does not delete another account's backups.");
        let candidates = self.candidates.clone();
        let selected = self.config.selected_steamid.clone();
        for c in &candidates {
            let is_sel = selected.as_deref() == Some(c.steamid.as_str());
            let label = format!(
                "{}  —  saved {}",
                c.steamid,
                c.modified.map_or_else(|| "unknown".into(), system_local)
            );
            if ui.radio(is_sel, label).clicked() && !is_sel {
                self.select_account(&c.steamid);
            }
        }
    }

    fn backups(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("💾 Back up now").clicked() {
                self.backup_now();
            }
            if let Some(dest) = &self.config.backup_dest
                && ui.button("📂 Open backup folder").clicked()
            {
                platform::open_folder(&snapshot::snapshots_dir(dest));
            }
        });
        ui.separator();

        if self.snapshots.is_empty() {
            ui.label("No snapshots yet for the selected account.");
            return;
        }

        let snaps = self.snapshots.clone();
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("snap_grid")
                .num_columns(5)
                .striped(true)
                .spacing([14.0, 4.0])
                .show(ui, |ui| {
                    ui.strong("When");
                    ui.strong("Age");
                    ui.strong("Size");
                    ui.strong("Reason");
                    ui.strong("");
                    ui.end_row();
                    for s in &snaps {
                        ui.label(local_time(s.metadata.created_utc));
                        ui.label(relative_age(s.metadata.created_utc));
                        ui.label(human_size(s.stored_size()));
                        ui.label(s.metadata.reason.label());
                        if ui.button("Open").clicked() {
                            platform::open_folder(&s.dir);
                        }
                        ui.end_row();
                    }
                });
        });
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

        ui.add_space(4.0);
        ui.checkbox(
            &mut self.config.pre_launch,
            "Snapshot before the game launches",
        );
        ui.checkbox(
            &mut self.config.periodic,
            "Snapshot periodically while playing",
        );
        ui.checkbox(&mut self.config.post_exit, "Snapshot after the game exits");

        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                self.apply_settings();
            }
            if ui.button("Open logs folder").clicked()
                && let Ok(dir) = paths::log_dir()
            {
                platform::open_folder(&dir);
            }
        });
        ui.label(format!(
            "Interval is clamped to at least {MIN_INTERVAL_SECS}s. Stage toggles apply the next time the game launches."
        ));
    }

    fn help(&mut self, ui: &mut egui::Ui) {
        ui.strong("Steam launch option");
        ui.label("Copy the command below into Steam > Elden Ring > Properties > General > Launch Options. This makes backups happen automatically every time you play, even with this window closed.");
        ui.add_space(4.0);

        let exe =
            std::env::current_exe().unwrap_or_else(|_| PathBuf::from("eldenring-backuptool.exe"));
        let mut cmd = launch::launch_option(&exe);
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
        ui.strong("How to restore a save (manual)");
        ui.label("1. Fully close Elden Ring, and preferably exit Steam completely.");
        ui.label("2. Open the Backups tab, pick a snapshot, and click Open.");
        ui.label("3. Double-click save.zip, then copy the .sl2 (and .sl2.bak) into your save folder, replacing the current ones.");
        ui.label("4. Reopen Steam. If it reports a Steam Cloud conflict, choose the LOCAL copy — not the newer cloud one.");
        if ui.button("📂 Open my save folder").clicked() {
            if let Some(c) = self.selected_candidate() {
                platform::open_folder(&c.dir);
            } else if let Some(root) = &self.elden_root {
                platform::open_folder(root);
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!(
                "Elden Ring Save Guard v{}",
                save_guard::APP_VERSION
            ));
            if ui
                .button("App updates…")
                .on_hover_text("Check for a new version and choose how often to check")
                .clicked()
            {
                self.update_window_open = true;
            }
        });
        ui.label("Does not modify the game, inject code, or touch Easy Anti-Cheat. It only copies your save files.");
    }
}

const GREEN: egui::Color32 = egui::Color32::from_rgb(80, 160, 100);
const AMBER: egui::Color32 = egui::Color32::from_rgb(200, 140, 60);

impl App {
    /// A small button in the header while a new version is out.
    fn update_chip(&mut self, ui: &mut egui::Ui) {
        let Some(release) = self.updater.notification(&self.config.app_updates) else {
            return;
        };
        let text = if self.updater.staged().is_some() {
            format!("Restart for v{}", release.version)
        } else {
            format!("Update v{} available", release.version)
        };
        let chip = egui::Button::new(egui::RichText::new(text).color(egui::Color32::WHITE))
            .fill(GREEN.gamma_multiply(0.8))
            .corner_radius(egui::CornerRadius::same(10));
        if ui
            .add(chip)
            .on_hover_text("A new version of Save Guard is out. Click to see what changed.")
            .clicked()
        {
            self.update_window_open = true;
        }
    }

    fn update_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut period = self.config.app_updates.period;
        let mut check = false;
        let mut action = None;
        // A restart closes the window, so it waits for a backup or a reload.
        let busy = self.pending.is_some();
        let updater = &self.updater;
        let settings = &self.config.app_updates;
        egui::Window::new("App updates")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(480.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(ctx.content_rect().height() - 220.0)
                    .show(ui, |ui| {
                        update_status(ui, updater, settings);
                    });
                ui.separator();
                update_schedule(ui, updater, settings, &mut period, &mut check);
                action = update_actions(ui, updater, busy);
            });
        if period != self.config.app_updates.period {
            self.config.app_updates.period = period;
            self.save_update_settings();
        }
        if check {
            self.updater.check_now();
        }
        match action {
            Some(UpdateAction::Download) => self.updater.download(),
            Some(UpdateAction::Restart) => {
                if self.updater.install_and_restart().is_ok() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Some(UpdateAction::Skip) => {
                self.updater.skip(&mut self.config.app_updates);
                self.save_update_settings();
                open = false;
            }
            Some(UpdateAction::Later) => {
                self.updater.dismiss();
                open = false;
            }
            None => {}
        }
        self.update_window_open = open;
    }
}

/// Where the running version stands, and the notes of every newer release.
fn update_status(ui: &mut egui::Ui, updater: &Updater, settings: &update::Settings) {
    let current = updater.current().map_or_else(
        || APP_INFO.version.to_owned(),
        |version| version.to_string(),
    );
    if let Some(found) = updater.update() {
        let latest = found.latest();
        ui.heading(format!("Version {} is available", latest.version));
        let released = if latest.date.is_empty() {
            String::new()
        } else {
            format!(" It was released on {}.", latest.date)
        };
        ui.label(format!("You have version {current}.{released}"));
        if updater.staged() == Some(latest.version) {
            ui.colored_label(
                GREEN,
                "Downloaded and verified. Restart Save Guard to finish.",
            );
        } else if settings.skipped.as_deref() == Some(latest.version.to_string().as_str()) {
            ui.weak("You skipped this version, so the header does not point it out.");
        }
        if !updater.can_install() {
            ui.label(
                "This release has no build that Save Guard can install by itself. \
                 Download it from the release page instead.",
            );
            ui.hyperlink_to("Release page", &latest.page_url);
        }
        ui.add_space(8.0);
        ui.strong("What's new");
        for release in found.releases() {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.strong(release.version.to_string());
                if !release.date.is_empty() {
                    ui.weak(&release.date);
                }
            });
            release_notes(ui, &release.notes);
        }
        ui.add_space(4.0);
        ui.hyperlink_to("All releases", APP_INFO.releases_page());
    } else if updater.is_checking() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Checking GitHub for a new version…");
        });
    } else if updater.is_up_to_date() {
        ui.colored_label(GREEN, format!("Version {current} is the latest version."));
    } else {
        ui.label(format!("You have version {current}."));
    }
    if let Some(error) = updater.error() {
        ui.add_space(6.0);
        ui.colored_label(AMBER, error);
    }
}

fn update_schedule(
    ui: &mut egui::Ui,
    updater: &Updater,
    settings: &update::Settings,
    period: &mut CheckPeriod,
    check: &mut bool,
) {
    ui.horizontal(|ui| {
        ui.label("Check for new versions");
        egui::ComboBox::from_id_salt("app-update-period")
            .selected_text(period.label())
            .show_ui(ui, |ui| {
                for option in CheckPeriod::ALL {
                    ui.selectable_value(period, option, option.label());
                }
            });
        *check = ui
            .add_enabled(!updater.is_busy(), egui::Button::new("⟳ Check now"))
            .clicked();
    });
    let last = settings.last_check.map_or_else(
        || "Never checked".to_owned(),
        |then| format!("Last checked {}", update::describe_age(then, update::now())),
    );
    ui.weak(format!(
        "{last}. A check asks GitHub for Save Guard's releases and sends nothing else. \
         The background monitor that Steam starts never checks."
    ));
}

fn release_notes(ui: &mut egui::Ui, markdown: &str) {
    let blocks = update::parse_notes(markdown);
    if blocks.is_empty() {
        ui.weak("No notes for this release.");
    }
    for block in blocks {
        match block {
            NoteBlock::Heading(text) => {
                ui.strong(text);
            }
            NoteBlock::Bullet(text) => {
                ui.horizontal_top(|ui| {
                    ui.label("•");
                    ui.add(egui::Label::new(text).wrap());
                });
            }
            NoteBlock::Paragraph(text) => {
                ui.add(egui::Label::new(text).wrap());
            }
        }
    }
}

fn update_actions(ui: &mut egui::Ui, updater: &Updater, busy: bool) -> Option<UpdateAction> {
    let found = updater.update()?;
    ui.add_space(6.0);
    if let Some((received, total)) = updater.progress() {
        let percent = total
            .filter(|total| *total > 0)
            .map_or(0, |total| received.saturating_mul(100) / total)
            .min(100);
        let fraction = f32::from(u8::try_from(percent).unwrap_or(100)) / 100.0;
        ui.add(egui::ProgressBar::new(fraction).text(format!(
            "Downloading {}… {percent}%",
            found.latest().version
        )));
        return None;
    }
    let mut action = None;
    ui.horizontal(|ui| {
        if updater.staged().is_some() {
            if ui
                .add_enabled(!busy, egui::Button::new("Restart to update"))
                .on_hover_text("Close Save Guard and open the new version")
                .on_disabled_hover_text("Wait for the backup in progress to finish")
                .clicked()
            {
                action = Some(UpdateAction::Restart);
            }
        } else if updater.can_install()
            && ui
                .add_enabled(!updater.is_busy(), egui::Button::new("Download update"))
                .on_hover_text(
                    "Download the new version and check it against its published SHA-256",
                )
                .clicked()
        {
            action = Some(UpdateAction::Download);
        }
        if ui
            .button("Skip this version")
            .on_hover_text("Stop pointing out this version. A newer one shows up again.")
            .clicked()
        {
            action = Some(UpdateAction::Skip);
        }
        if ui
            .button("Not now")
            .on_hover_text("Hide the reminder until Save Guard starts again")
            .clicked()
        {
            action = Some(UpdateAction::Later);
        }
    });
    action
}

fn dir_name(p: &std::path::Path) -> String {
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
    fn background_job_does_not_block_the_caller() {
        let (release, wait) = mpsc::channel();
        let job = spawn_job(egui::Context::default(), move || {
            wait.recv().unwrap();
            42
        })
        .unwrap();
        assert!(matches!(job.try_recv(), Err(TryRecvError::Empty)));
        release.send(()).unwrap();
        assert_eq!(
            job.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
            42
        );
    }
}
