//! Result-list columns: layout, the header (click to sort, drag to
//! resize, right-click to show or hide columns) and cell text.

use eframe::egui::{
    self, Color32, CursorIcon, FontFamily, FontId, Rect, Sense, Stroke, pos2, vec2,
};
use ferrow_index::{ColumnSet, EntryMetadata};

use super::FerrowApp;
use crate::settings::{ColumnId, SortChoice, default_columns};
use crate::theme::{self, Palette};

/// Smallest width a column can be dragged to.
const MIN_WIDTH: f32 = 60.0;
/// The flexible Folder column never shrinks below this.
const MIN_FLEX: f32 = 120.0;

/// One visible column with its horizontal span.
#[derive(Clone, Copy)]
pub(super) struct Placed {
    pub id: ColumnId,
    pub left: f32,
    pub right: f32,
}

/// Place the visible columns between `left` and `right`. Folder (or, if
/// hidden, the last column) takes the space the fixed columns leave.
pub(super) fn layout(app: &FerrowApp, left: f32, right: f32) -> Vec<Placed> {
    let visible: Vec<_> = app.settings.columns.iter().filter(|c| c.visible).collect();
    let flex = visible
        .iter()
        .position(|c| c.id == ColumnId::Folder)
        .unwrap_or(visible.len().saturating_sub(1));
    let fixed: f32 = visible
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != flex)
        .map(|(_, c)| c.width.max(MIN_WIDTH))
        .sum();
    let flex_w = (right - left - fixed).max(MIN_FLEX);
    let mut x = left;
    visible
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let w = if i == flex {
                flex_w
            } else {
                c.width.max(MIN_WIDTH)
            };
            let placed = Placed {
                id: c.id,
                left: x,
                right: x + w,
            };
            x += w;
            placed
        })
        .collect()
}

/// Whether the loaded index carries data for a column.
pub(super) fn column_present(present: ColumnSet, id: ColumnId) -> bool {
    let needed = match id {
        ColumnId::Size => ColumnSet::SIZE,
        ColumnId::Modified => ColumnSet::MODIFIED,
        ColumnId::Created => ColumnSet::CREATED,
        ColumnId::Accessed => ColumnSet::ACCESSED,
        ColumnId::Attributes => ColumnSet::ATTRIBUTES,
        _ => return true,
    };
    present.contains(needed)
}

impl FerrowApp {
    /// Draw the header and handle sort clicks, resizing and the
    /// show/hide menu. Returns true when the sort changed.
    pub(super) fn column_header(
        &mut self,
        ui: &mut egui::Ui,
        header: Rect,
        cols: &[Placed],
        present: ColumnSet,
    ) -> bool {
        let p = self.palette;
        let painter = ui.painter_at(header);
        let font = FontId::new(12.0, FontFamily::Name(theme::SEMIBOLD.into()));
        let mut sort_changed = false;
        let mut resized = false;
        for (i, col) in cols.iter().enumerate() {
            let cell = Rect::from_min_max(
                pos2(col.left, header.top()),
                pos2(col.right, header.bottom()),
            );
            let text_left = if col.id == ColumnId::Name {
                col.left + 28.0
            } else {
                col.left + 8.0
            };
            let missing = !column_present(present, col.id);
            let resp = ui.interact(cell, egui::Id::unique(("col-head", i)), Sense::click());
            if resp.hovered() && col.id.sort_key().is_some() {
                painter.rect_filled(cell.shrink2(vec2(2.0, 4.0)), 4, p.row_hover);
            }
            let color = if missing {
                p.text_tertiary
            } else {
                p.text_secondary
            };
            let galley = painter.layout_no_wrap(col.id.label().to_owned(), font.clone(), color);
            let label_w = galley.size().x;
            let align_right = col.id == ColumnId::Size;
            let label_x = if align_right {
                col.right - 10.0 - label_w
            } else {
                text_left
            };
            painter.galley(
                pos2(label_x, cell.center().y - galley.size().y / 2.0),
                galley,
                color,
            );
            if let Some(key) = col.id.sort_key()
                && key == self.settings.sort
            {
                let ax = if align_right {
                    label_x - 10.0
                } else {
                    label_x + label_w + 10.0
                };
                sort_arrow(
                    &painter,
                    pos2(ax, cell.center().y),
                    self.settings.sort_descending,
                    p.text_tertiary,
                );
            }
            let resp = if missing {
                resp.on_hover_text("Not in this index. Rebuild the index to fill this column.")
            } else {
                resp
            };
            if resp.clicked()
                && let Some(key) = col.id.sort_key()
            {
                if self.settings.sort == key {
                    self.settings.sort_descending = !self.settings.sort_descending;
                } else {
                    self.settings.sort = key;
                    // Dates and sizes are most useful newest/largest first.
                    self.settings.sort_descending = key != SortChoice::Name;
                }
                sort_changed = true;
            }
            // Divider and resize grip on the right edge (not after the last).
            if i + 1 < cols.len() {
                painter.line_segment(
                    [
                        pos2(col.right, header.top() + 8.0),
                        pos2(col.right, header.bottom() - 8.0),
                    ],
                    Stroke::new(1.0, p.divider),
                );
                let grip = Rect::from_center_size(
                    pos2(col.right, header.center().y),
                    vec2(8.0, header.height()),
                );
                let g = ui
                    .interact(grip, egui::Id::unique(("col-grip", i)), Sense::drag())
                    .on_hover_cursor(CursorIcon::ResizeColumn);
                if g.dragged()
                    && col.id != ColumnId::Folder
                    && let Some(cfg) = self.settings.columns.iter_mut().find(|c| c.id == col.id)
                {
                    cfg.width = (col.right - col.left + g.drag_delta().x).max(MIN_WIDTH);
                }
                if g.drag_stopped() {
                    resized = true;
                }
            }
        }
        // Right-click anywhere on the header: choose the columns.
        let all = ui.interact(header, egui::Id::unique("col-header-menu"), Sense::click());
        let mut columns_changed = false;
        all.context_menu(|ui| {
            ui.set_min_width(220.0);
            ui.label(
                egui::RichText::new("Columns")
                    .color(p.text_secondary)
                    .size(12.0),
            );
            for cfg in self.settings.columns.iter_mut() {
                let mut on = cfg.visible;
                let enabled = cfg.id != ColumnId::Name;
                if ui
                    .add_enabled(enabled, egui::Checkbox::new(&mut on, cfg.id.label()))
                    .changed()
                {
                    cfg.visible = on;
                    columns_changed = true;
                }
            }
            ui.separator();
            if ui.button("Reset columns").clicked() {
                self.settings.columns = default_columns();
                columns_changed = true;
                ui.close();
            }
        });
        if columns_changed {
            self.columns_changed();
        }
        if sort_changed || resized || columns_changed {
            self.save_settings();
        }
        painter.line_segment(
            [
                pos2(header.left(), header.bottom()),
                pos2(header.right(), header.bottom()),
            ],
            Stroke::new(1.0, p.divider),
        );
        sort_changed
    }

    /// A newly shown column may need metadata the index lacks.
    fn columns_changed(&mut self) {
        if let Some(index) = &self.index {
            let present = index.columns_present();
            let wanted = self.settings.index_columns();
            self.columns_need_rebuild = !present.contains(wanted);
        }
    }
}

fn sort_arrow(painter: &egui::Painter, c: egui::Pos2, descending: bool, color: Color32) {
    let s = Stroke::new(1.2, color);
    let (a, b) = if descending { (-1.5, 2.0) } else { (1.5, -2.0) };
    painter.line_segment([c + vec2(-3.5, a), c + vec2(0.0, b)], s);
    painter.line_segment([c + vec2(0.0, b), c + vec2(3.5, a)], s);
}

/// Text for a metadata cell; `None` when the cell is empty.
pub(super) fn cell_text(id: ColumnId, meta: &EntryMetadata, is_dir: bool) -> Option<String> {
    match id {
        ColumnId::Size => {
            if is_dir {
                None
            } else {
                meta.size.map(format_size)
            }
        }
        ColumnId::Modified => meta.modified.map(format_time),
        ColumnId::Created => meta.created.map(format_time),
        ColumnId::Accessed => meta.accessed.map(format_time),
        ColumnId::Attributes => meta.attributes.map(format_attributes),
        _ => None,
    }
}

/// Explorer-style size: KB with thousands separators below 1 GB.
pub(super) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 || u == 0 {
        format!("{:.0} {}", v, UNITS[u])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

/// Unix 100-ns ticks to a local "YYYY-MM-DD HH:MM".
pub(super) fn format_time(ticks: i64) -> String {
    use chrono::{Local, TimeZone};
    let secs = ticks.div_euclid(10_000_000);
    let nanos = (ticks.rem_euclid(10_000_000) * 100) as u32;
    match Local.timestamp_opt(secs, nanos) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => {
            t.format("%Y-%m-%d %H:%M").to_string()
        }
        chrono::LocalResult::None => "—".to_owned(),
    }
}

/// Windows attribute letters (R, H, S, A, C, E), or a Unix mode string.
pub(super) fn format_attributes(bits: u32) -> String {
    if cfg!(windows) {
        let mut s = String::new();
        for (bit, ch) in [
            (0x1, 'R'),
            (0x2, 'H'),
            (0x4, 'S'),
            (0x20, 'A'),
            (0x800, 'C'),
            (0x4000, 'E'),
        ] {
            if bits & bit != 0 {
                s.push(ch);
            }
        }
        s
    } else {
        let mut s = String::with_capacity(9);
        for shift in [6u32, 3, 0] {
            let m = (bits >> shift) & 7;
            s.push(if m & 4 != 0 { 'r' } else { '-' });
            s.push(if m & 2 != 0 { 'w' } else { '-' });
            s.push(if m & 1 != 0 { 'x' } else { '-' });
        }
        s
    }
}

/// Right-aligned columns.
pub(super) fn right_aligned(id: ColumnId) -> bool {
    id == ColumnId::Size
}

pub(super) fn missing_text(p: &Palette) -> (&'static str, Color32) {
    ("—", p.text_tertiary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "2 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_size(300 * 1024 * 1024), "300 MB");
        assert_eq!(format_size(u64::MAX), "16777216 TB");
    }

    #[test]
    fn times_are_formatted_and_never_panic() {
        // 2026-01-01T00:00:00Z in ticks; local formatting has 16 chars.
        let t = 1_767_225_600i64 * 10_000_000;
        assert_eq!(format_time(t).len(), 16);
        let _ = format_time(i64::MIN);
        let _ = format_time(i64::MAX);
        let _ = format_time(-1);
    }

    #[test]
    fn attributes() {
        if cfg!(windows) {
            assert_eq!(format_attributes(0x1 | 0x2 | 0x20), "RHA");
        } else {
            assert_eq!(format_attributes(0o754), "rwxr-xr--");
        }
    }
}
