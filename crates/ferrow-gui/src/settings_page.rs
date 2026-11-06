//! The settings page, laid out like Windows 11 Settings: a page title
//! and cards with one setting per row.

use std::path::PathBuf;

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontFamily, FontId, Layout, RichText, Sense, Stroke,
    StrokeKind, Vec2, vec2,
};

use super::{FerrowApp, Page, PickTarget};
use crate::platform;
use crate::settings::ThemeChoice;
use crate::theme;

const CARD_GAP: f32 = 4.0;
const SECTION_GAP: f32 = 20.0;
const MAX_WIDTH: f32 = 860.0;

impl FerrowApp {
    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        self.poll_folder_pick();
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
        // Indexed folders.
        card(ui, &p, |ui| {
            row_header(
                ui,
                &p,
                "Indexed folders",
                "Ferrow finds files and folders inside these locations.",
            );
            changed |= self.folder_list(ui, PickTarget::Root);
        });
        ui.add_space(CARD_GAP);
        card(ui, &p, |ui| {
            row_header(
                ui,
                &p,
                "Excluded folders",
                "Left out of the index, including everything inside them.",
            );
            changed |= self.folder_list(ui, PickTarget::Exclude);
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
                    let can = !self.settings.roots.is_empty() && self.scan_status.is_none();
                    let button = egui::Button::new(RichText::new("Rebuild now").color(if can {
                        Color32::WHITE
                    } else {
                        p.text_tertiary
                    }))
                    .fill(if can { p.accent } else { p.control })
                    .min_size(vec2(120.0, 32.0));
                    let resp = ui.add_enabled(can, button);
                    if self.settings.roots.is_empty() {
                        resp.on_disabled_hover_text("Add a folder to index first.");
                    } else if resp.clicked() {
                        self.rebuild_index();
                    }
                });
            });
        });
        if changed {
            self.save_settings();
        }
        if let Some(err) = &self.settings_error {
            ui.add_space(6.0);
            ui.label(RichText::new(err).color(Color32::from_rgb(0xE8, 0x5A, 0x4F)));
        }
    }

    /// Rows for one folder list, with remove buttons and an add button.
    /// Returns true when the list changed.
    fn folder_list(&mut self, ui: &mut egui::Ui, target: PickTarget) -> bool {
        let p = self.palette;
        let mut remove = None;
        let list = match target {
            PickTarget::Root => &self.settings.roots,
            PickTarget::Exclude => &self.settings.excludes,
        };
        ui.add_space(8.0);
        for (i, folder) in list.iter().enumerate() {
            let (rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
            let hovered = ui.rect_contains_pointer(rect);
            if hovered {
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(4), p.row_hover);
            }
            super::icon_folder(ui.painter(), rect.left_center() + vec2(14.0, 0.0), &p);
            let text_rect = egui::Rect::from_min_max(
                rect.left_top() + vec2(32.0, 0.0),
                rect.right_bottom() - vec2(90.0, 0.0),
            );
            let galley = super::ellipsized(
                ui,
                folder.display().to_string(),
                FontId::proportional(14.0),
                p.text,
                text_rect.width(),
            );
            ui.painter().galley(
                egui::pos2(text_rect.left(), rect.center().y - galley.size().y / 2.0),
                galley,
                p.text,
            );
            let btn = egui::Rect::from_min_size(
                egui::pos2(rect.right() - 84.0, rect.center().y - 14.0),
                vec2(80.0, 28.0),
            );
            let resp = ui.put(
                btn,
                egui::Button::new("Remove")
                    .fill(p.control)
                    .stroke(Stroke::new(1.0, p.control_stroke))
                    .min_size(btn.size()),
            );
            if resp.clicked() {
                remove = Some(i);
            }
        }
        if list.is_empty() {
            ui.label(RichText::new("None").color(p.text_tertiary));
        }
        ui.add_space(6.0);
        let picking = self.folder_pick.is_some();
        let add = ui.add_enabled(
            !picking,
            egui::Button::new(match target {
                PickTarget::Root => "Add folder…",
                PickTarget::Exclude => "Add excluded folder…",
            })
            .fill(p.control)
            .stroke(Stroke::new(1.0, p.control_stroke))
            .min_size(vec2(160.0, 32.0)),
        );
        if add.clicked() {
            self.pick_folder(ui.ctx(), target);
        }
        if let Some(i) = remove {
            match target {
                PickTarget::Root => self.settings.roots.remove(i),
                PickTarget::Exclude => self.settings.excludes.remove(i),
            };
            return true;
        }
        false
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
                        let b = egui::Button::new(text)
                            .fill(if selected { p.accent } else { p.control })
                            .stroke(Stroke::new(1.0, p.control_stroke))
                            .min_size(vec2(76.0, 30.0));
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
                            .add(
                                egui::Button::new("Open")
                                    .fill(p.control)
                                    .stroke(Stroke::new(1.0, p.control_stroke))
                                    .min_size(vec2(80.0, 30.0)),
                            )
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
            let list = match target {
                PickTarget::Root => &mut self.settings.roots,
                PickTarget::Exclude => &mut self.settings.excludes,
            };
            if !list.contains(&folder) {
                list.push(folder);
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
        if self.settings.roots.is_empty() {
            return;
        }
        self.engine.scan(ferrow_walk::WalkOptions {
            roots: self.settings.roots.clone(),
            excludes: self.settings.excludes.clone(),
            follow_symlinks: self.settings.follow_symlinks,
        });
    }

    pub(super) fn index_home(&mut self, home: PathBuf) {
        if !self.settings.roots.contains(&home) {
            self.settings.roots.push(home);
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
