//! The Elden Ring Save Guard desktop app: a compact dashboard for choosing the
//! save to protect, checking status, browsing snapshots, and copying the Steam
//! launch option. Restore is intentionally manual (see README).

use std::path::PathBuf;

use chrono::{DateTime, Local, Utc};

use save_guard::config::{self, Config, MAX_RETENTION, MIN_INTERVAL_SECS};
use save_guard::discovery::{self, SaveCandidate};
use save_guard::launch;
use save_guard::monitor::source_files;
use save_guard::paths;
use save_guard::platform;
use save_guard::snapshot::{self, Reason, Snapshot};

pub(crate) fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 560.0])
            .with_min_inner_size([560.0, 420.0])
            .with_title("Elden Ring Save Guard"),
        ..Default::default()
    };
    eframe::run_native(
        "Elden Ring Save Guard",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
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
}

impl App {
    fn new() -> Self {
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
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        self.candidates = match &self.elden_root {
            Some(root) => discovery::discover(root),
            None => Vec::new(),
        };
        self.reload_snapshots();
    }

    fn reload_snapshots(&mut self) {
        self.snapshots = match (&self.config.selected_steamid, &self.config.backup_dest) {
            (Some(steamid), Some(dest)) => {
                let mut s = snapshot::list(dest, steamid);
                s.reverse(); // newest first for display
                s
            }
            _ => Vec::new(),
        };
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
        let Some(candidate) = self.selected_candidate().cloned() else {
            self.status = Some((true, "Select a Steam account first.".into()));
            return;
        };
        let Some(dest) = self.config.backup_dest.clone() else {
            self.status = Some((true, "Choose a backup destination first.".into()));
            return;
        };
        if let Err(e) = paths::validate_backup_dest(&candidate.dir, &dest) {
            self.status = Some((true, e.to_string()));
            return;
        }
        let sources = source_files(&candidate);
        match snapshot::create(&dest, &candidate.steamid, &sources, Reason::Manual) {
            Ok(Some(snap)) => {
                let _ =
                    save_guard::retention::apply(&dest, &candidate.steamid, self.config.retention);
                self.status = Some((false, format!("Backed up: {}", dir_name(&snap.dir))));
            }
            Ok(None) => {
                self.status = Some((false, "Save unchanged — no new backup needed.".into()));
            }
            Err(e) => self.status = Some((true, format!("Backup failed: {e}"))),
        }
        self.reload_snapshots();
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
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("🛡 Elden Ring Save Guard");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Dashboard, "Dashboard");
                ui.selectable_value(&mut self.tab, Tab::Backups, "Backups");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                ui.selectable_value(&mut self.tab, Tab::Help, "Help");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⟳ Refresh").clicked() {
                        self.refresh();
                    }
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

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Dashboard => self.dashboard(ui),
            Tab::Backups => self.backups(ui),
            Tab::Settings => self.settings(ui),
            Tab::Help => self.help(ui),
        });
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
            ui.colored_label(color, "●");
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

                if let Some(dest) = &self.config.backup_dest
                    && let Some(free) = platform::free_space(dest)
                {
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
        ui.label("Copy the command below into Steam → Elden Ring → Properties → General → Launch Options. This makes backups happen automatically every time you play, even with this window closed.");
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
        ui.label(format!(
            "Elden Ring Save Guard v{}",
            save_guard::APP_VERSION
        ));
        ui.label("Does not modify the game, inject code, or touch Easy Anti-Cheat. It only copies your save files.");
    }
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
