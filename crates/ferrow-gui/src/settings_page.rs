//! The settings page, laid out like Windows 11 Settings: a page title
//! and cards with one setting per row.

use std::path::PathBuf;

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontFamily, FontId, Layout, RichText, Sense, Stroke,
    StrokeKind, Vec2, vec2,
};

use super::{FerrowApp, Page, PickTarget};
use crate::platform;
use crate::settings::{SourceKind, ThemeChoice, UpdateChannel};
use crate::theme;
use crate::update::{self, UpdateState};

const CARD_GAP: f32 = 4.0;
const SECTION_GAP: f32 = 20.0;
const MAX_WIDTH: f32 = 860.0;

impl FerrowApp {
    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        self.poll_folder_pick();
        self.poll_drives();
        let outer = ui.max_rect();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = outer.width().min(MAX_WIDTH);
                let pad = ((outer.width() - width) / 2.0).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.vertical(|ui| {
                        ui.set_width(width - 24.0);
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if back_button(ui, &p).clicked() {
                                self.page = Page::Results;
                                self.focus_search = true;
                            }
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("Settings")
                                    .font(FontId::new(
                                        28.0,
                                        FontFamily::Name(theme::SEMIBOLD.into()),
                                    ))
                                    .color(p.text),
                            );
                        });
                        ui.add_space(SECTION_GAP);
                        self.index_section(ui);
                        ui.add_space(SECTION_GAP);
                        self.appearance_section(ui);
                        ui.add_space(SECTION_GAP);
                        self.updates_section(ui);
                        ui.add_space(SECTION_GAP);
                        self.about_section(ui);
                        ui.add_space(SECTION_GAP);
                    });
                });
            });
    }

    fn index_section(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        section_title(ui, &p, "Index");

        let mut changed = false;
        card(ui, &p, |ui| {
            row_header(
                ui,
                &p,
                "Locations",
                "Drives, folders, network drives and network folders that Ferrow searches.",
            );
            changed |= self.location_list(ui);
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let picking = self.folder_pick.is_some();
                if ui
                    .add_enabled(!picking, std_button(&p, "Add folder…", 140.0))
                    .clicked()
                {
                    self.pick_folder(ui.ctx(), PickTarget::Root);
                }
                if cfg!(windows) {
                    let resp = ui.add(std_button(&p, "Add drive ▾", 130.0));
                    if resp.clicked() && self.drives.is_none() {
                        self.load_drives(ui.ctx());
                    }
                    let mut added = None;
                    egui::Popup::menu(&resp).show(|ui| {
                        ui.set_min_width(300.0);
                        match &self.drives {
                            None => {
                                ui.label(
                                    RichText::new("Looking for drives…").color(p.text_secondary),
                                );
                            }
                            Some(drives) if drives.is_empty() => {
                                ui.label(RichText::new("No drives found").color(p.text_secondary));
                            }
                            Some(drives) => {
                                for d in drives {
                                    let listed =
                                        self.settings.sources.iter().any(|s| s.path == d.root);
                                    let mut text =
                                        format!("{}   {}", d.root.display(), d.kind_label);
                                    if !d.detail.is_empty() {
                                        text.push_str(&format!("  ·  {}", d.detail));
                                    }
                                    let b = egui::Button::new(text).min_size(vec2(290.0, 30.0));
                                    if ui.add_enabled(!listed, b).clicked() {
                                        added = Some(d.root.clone());
                                    }
                                }
                            }
                        }
                    });
                    if let Some(root) = added {
                        changed |= self.settings.add_source(root);
                        self.drives = None; // refresh next time
                    }
                }
                if ui
                    .add(std_button(&p, "Add network location…", 190.0))
                    .clicked()
                {
                    self.network_input = Some(String::new());
                }
            });
            if let Some(text) = &mut self.network_input {
                ui.add_space(8.0);
                let mut add = false;
                let mut cancel = false;
                ui.horizontal(|ui| {
                    let hint = if cfg!(windows) {
                        r"\\server\share\folder"
                    } else {
                        "/mnt/share"
                    };
                    let edit = egui::TextEdit::singleline(text)
                        .hint_text(hint)
                        .desired_width(360.0)
                        .margin(vec2(8.0, 6.0));
                    let r = ui.add(edit);
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        add = true;
                    }
                    if ui
                        .add(accent_button(&p, "Add", 70.0, !text.trim().is_empty()))
                        .clicked()
                    {
                        add = true;
                    }
                    if ui.add(std_button(&p, "Cancel", 80.0)).clicked() {
                        cancel = true;
                    }
                });
                if add && !text.trim().is_empty() {
                    let path = PathBuf::from(text.trim());
                    changed |= self.settings.add_source(path);
                    cancel = true;
                }
                if cancel {
                    self.network_input = None;
                }
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Network locations are read with your account. They don't update live; \
                     rebuild them, or set an automatic rebuild below.",
                )
                .size(12.0)
                .color(p.text_tertiary),
            );
        });
        ui.add_space(CARD_GAP);
        card(ui, &p, |ui| {
            row_header(
                ui,
                &p,
                "Excluded folders",
                "Left out of the index, including everything inside them.",
            );
            changed |= self.exclude_list(ui);
            ui.add_space(6.0);
            let picking = self.folder_pick.is_some();
            if ui
                .add_enabled(!picking, std_button(&p, "Add excluded folder…", 190.0))
                .clicked()
            {
                self.pick_folder(ui.ctx(), PickTarget::Exclude);
            }
        });
        ui.add_space(CARD_GAP);
        card(ui, &p, |ui| {
            ui.horizontal(|ui| {
                row_text(
                    ui,
                    &p,
                    "Follow symbolic links",
                    "Also index folders that links and junctions point to.",
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if toggle(ui, &p, &mut self.settings.follow_symlinks).changed() {
                        changed = true;
                    }
                });
            });
        });
        ui.add_space(CARD_GAP);
        card(ui, &p, |ui| {
            ui.horizontal(|ui| {
                row_text(
                    ui,
                    &p,
                    "Rebuild automatically",
                    "Off by default, so Ferrow does no work while idle.",
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let current = self.settings.auto_rebuild;
                    egui::ComboBox::from_id_salt("auto-rebuild")
                        .selected_text(current.label())
                        .width(180.0)
                        .show_ui(ui, |ui| {
                            for choice in crate::settings::AutoRebuild::ALL {
                                if ui
                                    .selectable_label(current == choice, choice.label())
                                    .clicked()
                                {
                                    self.settings.auto_rebuild = choice;
                                    changed = true;
                                }
                            }
                        });
                });
            });
        });
        ui.add_space(CARD_GAP);
        if let Some(entries) = self.service_entries {
            card(ui, &p, |ui| {
                ui.horizontal(|ui| {
                    row_text(
                        ui,
                        &p,
                        "Index",
                        &format!(
                            "The Ferrow service indexes every drive ({} entries) and keeps it up to date as files change. Nothing to rebuild.",
                            super::group(entries as usize)
                        ),
                    );
                });
            });
        } else {
            card(ui, &p, |ui| {
                ui.horizontal(|ui| {
                    let detail = match (&self.scan_status, &self.index) {
                        (Some(s), _) => s.clone(),
                        (None, Some(index)) => format!(
                            "{} entries, loaded in {}.",
                            super::group(index.len()),
                            super::fmt_ms(self.load_time)
                        ),
                        (None, None) => "No index yet.".to_owned(),
                    };
                    row_text(ui, &p, "Rebuild index", &detail);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let can = self.settings.has_locations() && self.scan_status.is_none();
                        let resp =
                            ui.add_enabled(can, accent_button(&p, "Rebuild now", 120.0, can));
                        if !self.settings.has_locations() {
                            resp.on_disabled_hover_text("Add a location to index first.");
                        } else if resp.clicked() {
                            self.rebuild_index();
                        }
                    });
                });
            });
        }

        let mut winsearch_changed = false;
        if cfg!(windows) {
            ui.add_space(SECTION_GAP);
            section_title(ui, &p, "Windows");
            card(ui, &p, |ui| {
                ui.horizontal(|ui| {
                    row_text(
                        ui,
                        &p,
                        "Index all internal drives",
                        "Every fixed drive, including drives you add later. Removable and network drives are not included.",
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if toggle(ui, &p, &mut self.settings.windows.all_fixed_drives).changed() {
                            changed = true;
                        }
                    });
                });
            });
            ui.add_space(CARD_GAP);
            card(ui, &p, |ui| {
                ui.horizontal(|ui| {
                    let detail = if self.settings.windows.windows_search {
                        let status = ferrow_winsearch::status();
                        if let Some(error) = status.last_error {
                            format!("On, but not active: {error}")
                        } else if status.attached_pid.is_some() {
                            match status.source {
                                Some(ferrow_winsearch::ipc::IndexSource::Service) => {
                                    "On: Start menu searches use the Ferrow service index."
                                        .to_owned()
                                }
                                Some(ferrow_winsearch::ipc::IndexSource::Local) => {
                                    "On: Start menu searches use this app's index.".to_owned()
                                }
                                None => {
                                    "On: Start menu searches use Ferrow's index.".to_owned()
                                }
                            }
                        } else {
                            "On: waiting to connect to Windows Search…".to_owned()
                        }
                    } else {
                        "Use Ferrow's index to answer file and folder searches in the Windows Start menu."
                            .to_owned()
                    };
                    row_text(ui, &p, "Power Windows Search with Ferrow", &detail);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if toggle(ui, &p, &mut self.settings.windows.windows_search).changed() {
                            changed = true;
                            winsearch_changed = true;
                        }
                    });
                });
            });
            ui.add_space(CARD_GAP);
            card(ui, &p, |ui| {
                ui.horizontal(|ui| {
                    let on = self.service_entries.is_some();
                    row_text(
                        ui,
                        &p,
                        "Fast NTFS index and live updates",
                        if on {
                            "On: the Ferrow service reads each drive's NTFS file table and follows its change journal, so new, renamed and deleted files show up within a second."
                        } else {
                            "Off: install the Ferrow index service with Setup to index all drives in seconds and keep the index current by itself."
                        },
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // Follows whether the service answers; not a choice.
                        let mut state = on;
                        ui.add_enabled_ui(false, |ui| toggle(ui, &p, &mut state));
                    });
                });
            });
        }

        if changed {
            self.save_settings();
        }
        if winsearch_changed {
            self.sync_windows_search();
        }
        if let Some(err) = &self.settings_error {
            ui.add_space(6.0);
            ui.label(RichText::new(err).color(Color32::from_rgb(0xE8, 0x5A, 0x4F)));
        }
    }

    /// Rows of index locations with their kind and a remove button.
    /// Returns true when the list changed.
    fn location_list(&mut self, ui: &mut egui::Ui) -> bool {
        let p = self.palette;
        ui.add_space(8.0);
        let mut remove = None;
        for (i, source) in self.settings.sources.iter().enumerate() {
            let rect = list_row(
                ui,
                &p,
                source.kind,
                &source.path.display().to_string(),
                source.kind.label(),
            );
            if remove_button(ui, &p, rect).clicked() {
                remove = Some(i);
            }
        }
        if cfg!(windows) && self.settings.windows.all_fixed_drives {
            list_row(
                ui,
                &p,
                SourceKind::Drive,
                "All internal drives",
                "From the Windows option below",
            );
        }
        if !self.settings.has_locations() {
            ui.label(RichText::new("No locations yet.").color(p.text_tertiary));
        }
        if let Some(i) = remove {
            self.settings.sources.remove(i);
            return true;
        }
        false
    }

    fn exclude_list(&mut self, ui: &mut egui::Ui) -> bool {
        let p = self.palette;
        ui.add_space(8.0);
        let mut remove = None;
        for (i, path) in self.settings.excludes.iter().enumerate() {
            let rect = list_row(ui, &p, SourceKind::Folder, &path.display().to_string(), "");
            if remove_button(ui, &p, rect).clicked() {
                remove = Some(i);
            }
        }
        if self.settings.excludes.is_empty() {
            ui.label(RichText::new("None").color(p.text_tertiary));
        }
        if let Some(i) = remove {
            self.settings.excludes.remove(i);
            return true;
        }
        false
    }

    fn load_drives(&mut self, ctx: &egui::Context) {
        // Drive queries can stall on a sleeping or disconnected drive, so
        // they run on their own thread.
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("ferrow-drives".into())
            .spawn(move || {
                let _ = tx.send(platform::list_drives());
                wake.request_repaint();
            });
        if spawned.is_ok() {
            self.drives_rx = Some(rx);
        }
    }

    fn poll_drives(&mut self) {
        if let Some(rx) = &self.drives_rx
            && let Ok(list) = rx.try_recv()
        {
            self.drives = Some(list);
            self.drives_rx = None;
        }
    }

    fn appearance_section(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        section_title(ui, &p, "Appearance");
        card(ui, &p, |ui| {
            ui.horizontal(|ui| {
                row_text(
                    ui,
                    &p,
                    "Theme",
                    "Follow Windows, or always use light or dark.",
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut choice = self.settings.theme;
                    for (value, label) in [
                        (ThemeChoice::Dark, "Dark"),
                        (ThemeChoice::Light, "Light"),
                        (ThemeChoice::System, "System"),
                    ] {
                        let selected = choice == value;
                        let text = RichText::new(label).color(if selected {
                            Color32::WHITE
                        } else {
                            p.text
                        });
                        let b = egui::Button::new(text).min_size(vec2(76.0, 30.0));
                        let b = if selected {
                            theme::FilledButton::accent(&p, b)
                        } else {
                            theme::FilledButton::standard(&p, b)
                        };
                        if ui.add(b).clicked() {
                            choice = value;
                        }
                    }
                    if choice != self.settings.theme {
                        self.settings.theme = choice;
                        apply_theme_choice(ui.ctx(), choice);
                        self.save_settings();
                    }
                });
            });
        });
    }

    /// Installed copies check once when they open, at most once a day.
    pub(super) fn updates_on_open(&mut self, ctx: &egui::Context) {
        if self.updater.opened {
            return;
        }
        self.updater.opened = true;
        if !update::can_install() {
            return;
        }
        let now = unix_now();
        if self
            .settings
            .last_update_check
            .is_some_and(|t| now.saturating_sub(t) < 24 * 3600)
        {
            return;
        }
        self.check_for_updates(ctx);
    }

    fn check_for_updates(&mut self, ctx: &egui::Context) {
        let channel = self
            .settings
            .update_channel
            .resolve(&update::current_version());
        let wake = ctx.clone();
        self.updater.check(channel, move || wake.request_repaint());
        self.settings.last_update_check = Some(unix_now());
        self.save_settings();
    }

    fn updates_section(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        let ctx = ui.ctx().clone();
        let current = update::current_version();
        section_title(ui, &p, "Updates");
        card(ui, &p, |ui| {
            ui.horizontal(|ui| {
                let (title, detail) = match &self.updater.state {
                    UpdateState::Idle => (
                        format!("Ferrow {current}"),
                        "Not checked yet in this session.".to_owned(),
                    ),
                    UpdateState::Checking => (
                        format!("Ferrow {current}"),
                        "Checking for updates…".to_owned(),
                    ),
                    UpdateState::UpToDate => {
                        (format!("Ferrow {current}"), "You're up to date.".to_owned())
                    }
                    UpdateState::Available(r) => (
                        format!("Ferrow {} is available", r.version),
                        format!("You have {current}."),
                    ),
                    UpdateState::CheckFailed(e) => (
                        format!("Ferrow {current}"),
                        format!("The check failed: {e}"),
                    ),
                    UpdateState::Installing => (
                        "Installing the update…".to_owned(),
                        "Approve the administrator prompt. Ferrow closes and reopens.".to_owned(),
                    ),
                    UpdateState::InstallFailed(e) => {
                        ("The update was not installed".to_owned(), e.clone())
                    }
                };
                row_text(ui, &p, &title, &detail);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let busy = self.updater.busy();
                    if let UpdateState::Available(release) = &self.updater.state {
                        let release = release.clone();
                        if update::can_install() && release.has_setup {
                            if ui
                                .add_enabled(
                                    !busy,
                                    accent_button(&p, "Install update", 130.0, !busy),
                                )
                                .clicked()
                            {
                                let wake = ctx.clone();
                                self.updater
                                    .install(&release.version, move || wake.request_repaint());
                            }
                        } else if ui.add(std_button(&p, "Download", 100.0)).clicked() {
                            platform::open_url(&ferrow_update::release_page(&release.version));
                        }
                    }
                    if ui
                        .add_enabled(!busy, std_button(&p, "Check now", 100.0))
                        .clicked()
                    {
                        self.check_for_updates(&ctx);
                    }
                });
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let auto = match UpdateChannel::Auto.resolve(&current) {
                    ferrow_update::Channel::Beta => "Automatic (Beta)",
                    ferrow_update::Channel::Stable => "Automatic (Stable)",
                };
                row_text(
                    ui,
                    &p,
                    "Update channel",
                    "Stable gets tested releases. Beta also gets previews of the next version.",
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut choice = self.settings.update_channel;
                    egui::ComboBox::from_id_salt("update-channel")
                        .width(170.0)
                        .selected_text(match choice {
                            UpdateChannel::Auto => auto,
                            UpdateChannel::Stable => "Stable",
                            UpdateChannel::Beta => "Beta",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut choice, UpdateChannel::Auto, auto);
                            ui.selectable_value(&mut choice, UpdateChannel::Stable, "Stable");
                            ui.selectable_value(&mut choice, UpdateChannel::Beta, "Beta");
                        });
                    if choice != self.settings.update_channel && !self.updater.busy() {
                        self.settings.update_channel = choice;
                        // A release found on the other channel is no longer offered.
                        self.updater.state = UpdateState::Idle;
                        self.save_settings();
                    }
                });
            });
        });
    }

    fn about_section(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        section_title(ui, &p, "About");
        card(ui, &p, |ui| {
            ui.horizontal(|ui| {
                row_text(
                    ui,
                    &p,
                    &format!("Ferrow {}", env!("CARGO_PKG_VERSION")),
                    "Open source under the MIT licence.",
                );
            });
            ui.add_space(10.0);
            let index_path = crate::engine::default_index_path();
            if let Some(path) = &index_path {
                ui.horizontal(|ui| {
                    row_text(
                        ui,
                        &p,
                        "Data folder",
                        &path.parent().unwrap_or(path).display().to_string(),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(theme::FilledButton::standard(
                                &p,
                                egui::Button::new("Open").min_size(vec2(80.0, 30.0)),
                            ))
                            .clicked()
                            && let Some(dir) = path.parent()
                        {
                            platform::open(dir);
                        }
                    });
                });
            }
        });
    }

    fn pick_folder(&mut self, ctx: &egui::Context, target: PickTarget) {
        // The system dialog runs on its own thread, so the window keeps
        // painting while it is open.
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = ctx.clone();
        let title = match target {
            PickTarget::Root => "Choose a folder to index",
            PickTarget::Exclude => "Choose a folder to exclude",
        };
        let spawned = std::thread::Builder::new()
            .name("ferrow-folder-dialog".into())
            .spawn(move || {
                let folder = rfd::FileDialog::new().set_title(title).pick_folder();
                let _ = tx.send(folder);
                wake.request_repaint();
            });
        if spawned.is_ok() {
            self.folder_pick = Some((target, rx));
        }
    }

    fn poll_folder_pick(&mut self) {
        let Some((target, rx)) = &self.folder_pick else {
            return;
        };
        let target = *target;
        let Ok(result) = rx.try_recv() else { return };
        self.folder_pick = None;
        if let Some(folder) = result {
            let added = match target {
                PickTarget::Root => self.settings.add_source(folder),
                PickTarget::Exclude => {
                    let new = !self.settings.excludes.contains(&folder);
                    if new {
                        self.settings.excludes.push(folder);
                    }
                    new
                }
            };
            if added {
                self.save_settings();
            }
        }
    }

    pub(super) fn save_settings(&mut self) {
        self.settings_error = self
            .settings
            .save()
            .err()
            .map(|e| format!("Settings could not be saved: {e}"));
    }

    pub(super) fn rebuild_index(&mut self) {
        if self.service_entries.is_some() {
            // The service's index is live; a local scan would go unused.
            return;
        }
        let fixed = if self.settings.windows.all_fixed_drives {
            platform::fixed_drive_roots()
        } else {
            Vec::new()
        };
        let roots = self.settings.scan_roots(&fixed);
        if roots.is_empty() {
            return;
        }
        self.engine.scan(ferrow_walk::WalkOptions {
            roots,
            excludes: self.settings.excludes.clone(),
            follow_symlinks: self.settings.follow_symlinks,
            columns: self.settings.index_columns(),
            ntfs: ferrow_walk::NtfsMode::VolumeRoots,
        });
        self.settings.last_rebuild = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());
        self.save_settings();
    }

    pub(super) fn index_home(&mut self, home: PathBuf) {
        if self.settings.add_source(home) {
            self.save_settings();
        }
        self.rebuild_index();
    }
}

pub fn apply_theme_choice(ctx: &egui::Context, choice: ThemeChoice) {
    ctx.set_theme(match choice {
        ThemeChoice::System => egui::ThemePreference::System,
        ThemeChoice::Light => egui::ThemePreference::Light,
        ThemeChoice::Dark => egui::ThemePreference::Dark,
    });
}

fn back_button(ui: &mut egui::Ui, p: &theme::Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), p.row_hover);
    }
    let font = FontId::new(14.0, FontFamily::Name(theme::ICONS.into()));
    if theme::has_icon_font(ui.ctx()) {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "\u{E72B}",
            font,
            p.text,
        );
    } else {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "<",
            FontId::proportional(18.0),
            p.text,
        );
    }
    resp.on_hover_text("Back (Esc)")
}

fn section_title(ui: &mut egui::Ui, p: &theme::Palette, text: &str) {
    ui.label(
        RichText::new(text)
            .font(FontId::new(14.0, FontFamily::Name(theme::SEMIBOLD.into())))
            .color(p.text),
    );
    ui.add_space(6.0);
}

fn card(ui: &mut egui::Ui, p: &theme::Palette, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.card_stroke))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn row_header(ui: &mut egui::Ui, p: &theme::Palette, title: &str, detail: &str) {
    ui.label(RichText::new(title).size(14.0).color(p.text));
    ui.label(RichText::new(detail).size(12.0).color(p.text_secondary));
}

fn row_text(ui: &mut egui::Ui, p: &theme::Palette, title: &str, detail: &str) {
    ui.vertical(|ui| {
        row_header(ui, p, title, detail);
    });
}

/// Windows 11 style toggle switch.
fn toggle(ui: &mut egui::Ui, p: &theme::Palette, on: &mut bool) -> egui::Response {
    let size = Vec2::new(40.0, 20.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let painter = ui.painter();
    let radius = CornerRadius::same(10);
    if *on {
        painter.rect_filled(rect, radius, p.accent);
        painter.circle_filled(rect.right_center() - vec2(10.0, 0.0), 6.0, Color32::WHITE);
    } else {
        painter.rect(
            rect,
            radius,
            Color32::TRANSPARENT,
            Stroke::new(1.0, p.text_secondary),
            StrokeKind::Inside,
        );
        painter.circle_filled(rect.left_center() + vec2(10.0, 0.0), 5.0, p.text_secondary);
    }
    resp
}

fn std_button(p: &theme::Palette, text: &str, width: f32) -> theme::FilledButton<'static> {
    theme::FilledButton::standard(
        p,
        egui::Button::new(text.to_owned()).min_size(vec2(width, 32.0)),
    )
}

fn accent_button(
    p: &theme::Palette,
    text: &str,
    width: f32,
    enabled: bool,
) -> theme::FilledButton<'static> {
    if !enabled {
        let label = RichText::new(text.to_owned()).color(p.text_tertiary);
        return theme::FilledButton::standard(
            p,
            egui::Button::new(label).min_size(vec2(width, 32.0)),
        );
    }
    let label = RichText::new(text.to_owned()).color(Color32::WHITE);
    theme::FilledButton::accent(p, egui::Button::new(label).min_size(vec2(width, 32.0)))
}

/// One list row with a kind icon, the main text and a secondary label.
/// Returns the row rect (the remove button goes on its right).
fn list_row(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    kind: SourceKind,
    text: &str,
    detail: &str,
) -> egui::Rect {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
    if ui.rect_contains_pointer(rect) {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), p.row_hover);
    }
    let icon_c = rect.left_center() + vec2(14.0, 0.0);
    let glyph = match kind {
        SourceKind::Drive => Some("\u{EDA2}"),
        SourceKind::NetworkDrive => Some("\u{E8CE}"),
        SourceKind::NetworkFolder => Some("\u{E774}"),
        SourceKind::Folder => None,
    };
    match glyph {
        Some(g) if theme::has_icon_font(ui.ctx()) => {
            ui.painter().text(
                icon_c,
                egui::Align2::CENTER_CENTER,
                g,
                FontId::new(16.0, FontFamily::Name(theme::ICONS.into())),
                p.text_secondary,
            );
        }
        _ => super::icon_folder(ui.painter(), icon_c, p),
    }
    let text_left = rect.left() + 34.0;
    let text_right = rect.right() - 100.0;
    let detail_w = if detail.is_empty() { 0.0 } else { 120.0 };
    let main = super::ellipsized(
        ui,
        text.to_owned(),
        FontId::proportional(14.0),
        p.text,
        text_right - text_left - detail_w,
    );
    ui.painter().galley(
        egui::pos2(text_left, rect.center().y - main.size().y / 2.0),
        main,
        p.text,
    );
    if !detail.is_empty() {
        ui.painter().text(
            egui::pos2(text_right - 8.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            detail,
            FontId::proportional(12.0),
            p.text_tertiary,
        );
    }
    rect
}

fn remove_button(ui: &mut egui::Ui, p: &theme::Palette, row: egui::Rect) -> egui::Response {
    let btn = egui::Rect::from_min_size(
        egui::pos2(row.right() - 88.0, row.center().y - 15.0),
        vec2(84.0, 30.0),
    );
    ui.put(btn, std_button(p, "Remove", 84.0))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
