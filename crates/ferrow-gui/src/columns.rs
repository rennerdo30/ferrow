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
/// The flexible Folder column never shrinks below this: a folder you cannot
/// read is no use. Other columns give way first.
const MIN_FOLDER: f32 = 160.0;
/// Smallest the flexible column gets when it is not Folder.
const MIN_FLEX: f32 = 120.0;

/// One visible column with its horizontal span.
#[derive(Clone, Copy)]
pub(super) struct Placed {
    pub id: ColumnId,
    pub left: f32,
    pub right: f32,
}

/// The least width a column keeps before it is left out of a narrow window:
/// enough to show its content.
fn useful_width(id: ColumnId) -> f32 {
    match id {
        ColumnId::Name => 220.0,
        ColumnId::Size => 72.0,
        ColumnId::Modified | ColumnId::Created | ColumnId::Accessed => 128.0,
        ColumnId::Type => 84.0,
        _ => MIN_WIDTH,
    }
}

/// Place the visible columns between `left` and `right`. Folder (or, if
/// hidden, the last column) takes the space the fixed columns leave, and is
/// never narrower than [`MIN_FOLDER`]. When the window is too narrow for
/// that, the other columns give way, right to left: first they shrink to a
/// useful minimum, then Name shrinks, and only then are columns left out of
/// the view (the saved column choice is untouched).
pub(super) fn layout(app: &FerrowApp, left: f32, right: f32) -> Vec<Placed> {
    let visible: Vec<(ColumnId, f32)> = app
        .settings
        .columns
        .iter()
        .filter(|c| c.visible)
        .map(|c| (c.id, c.width))
        .collect();
    place(&visible, left, right)
}

fn place(visible: &[(ColumnId, f32)], left: f32, right: f32) -> Vec<Placed> {
    let flex = visible
        .iter()
        .position(|c| c.0 == ColumnId::Folder)
        .unwrap_or(visible.len().saturating_sub(1));
    let min_flex = match visible.get(flex) {
        Some((ColumnId::Folder, _)) => MIN_FOLDER,
        _ => MIN_FLEX,
    };
    let room = (right - left - min_flex).max(0.0);
    // Fixed columns: (position, column, width they would like).
    let mut fixed: Vec<(usize, ColumnId, f32)> = visible
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != flex)
        .map(|(i, c)| (i, c.0, c.1.max(MIN_WIDTH)))
        .collect();
    // Leave columns out (rightmost first, never Name) while even their
    // useful minimums do not fit.
    while fixed.iter().map(|c| useful_width(c.1)).sum::<f32>() > room {
        match fixed.iter().rposition(|c| c.1 != ColumnId::Name) {
            Some(at) => {
                fixed.remove(at);
            }
            None => break,
        }
    }
    // In a narrow view Name stops at its useful width: what it does not need
    // goes to Folder, whose end (the folder that tells rows apart) is what a
    // narrow window loses first.
    if fixed.iter().map(|c| c.2).sum::<f32>() > room {
        for c in fixed.iter_mut().filter(|c| c.1 == ColumnId::Name) {
            c.2 = c.2.min(useful_width(ColumnId::Name));
        }
    }
    // Shrink the rest, right to left, Name last.
    let mut excess = fixed.iter().map(|c| c.2).sum::<f32>() - room;
    let order: Vec<usize> = (0..fixed.len())
        .rev()
        .filter(|&i| fixed[i].1 != ColumnId::Name)
        .chain((0..fixed.len()).filter(|&i| fixed[i].1 == ColumnId::Name))
        .collect();
    for i in order {
        if excess <= 0.0 {
            break;
        }
        let min = useful_width(fixed[i].1).min(fixed[i].2);
        let cut = (fixed[i].2 - min).min(excess);
        fixed[i].2 -= cut;
        excess -= cut;
    }
    let fixed_sum: f32 = fixed.iter().map(|c| c.2).sum();
    let flex_w = (right - left - fixed_sum).max(min_flex);
    let mut x = left;
    let mut out = Vec::with_capacity(fixed.len() + 1);
    for (i, (id, _)) in visible.iter().enumerate() {
        let w = if i == flex {
            flex_w
        } else if let Some(c) = fixed.iter().find(|c| c.0 == i) {
            c.2
        } else {
            continue; // left out of this narrow view
        };
        out.push(Placed {
            id: *id,
            left: x,
            right: x + w,
        });
        x += w;
    }
    out
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
        // Right-click anywhere on the header: choose the columns. Registered
        // first so it lies *below* the cells: egui gives a click to the
        // last-registered widget under the pointer, and a header-wide area
        // added after the cells swallowed every sort click.
        let mut all = ui.interact(header, egui::Id::unique("col-header-menu"), Sense::click());
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
            // A right-click on a cell opens the same column menu.
            all = all.union(resp.clone());
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
            // Divider on the right edge (not after the last column).
            if i + 1 < cols.len() {
                painter.line_segment(
                    [
                        pos2(col.right, header.top() + 8.0),
                        pos2(col.right, header.bottom() - 8.0),
                    ],
                    Stroke::new(1.0, p.divider),
                );
            }
        }
        // Resize grips, registered after every header cell so they lie on
        // top: egui gives the pointer to the last-registered widget, and a
        // cell registered later covered half of each grip.
        for (i, pair) in cols.windows(2).enumerate() {
            let (left_col, right_col) = (&pair[0], &pair[1]);
            let grip = Rect::from_center_size(
                pos2(left_col.right, header.center().y),
                vec2(10.0, header.height()),
            );
            let g = ui
                .interact(grip, egui::Id::unique(("col-grip", i)), Sense::drag())
                .on_hover_cursor(CursorIcon::ResizeColumn);
            if g.dragged() {
                let delta = g.drag_delta().x;
                // The flexible Folder column takes whatever space is left,
                // so a divider next to it resizes the column on its other
                // side: dragging right of Folder moves that column's left
                // edge.
                let (target, width) = if left_col.id != ColumnId::Folder {
                    (left_col.id, left_col.right - left_col.left + delta)
                } else {
                    (right_col.id, right_col.right - right_col.left - delta)
                };
                if let Some(cfg) = self.settings.columns.iter_mut().find(|c| c.id == target) {
                    cfg.width = width.max(MIN_WIDTH);
                }
            }
            if g.drag_stopped() {
                resized = true;
            }
        }
        let mut columns_changed = false;
        all.context_menu(|ui| {
            ui.set_min_width(220.0);
            ui.label(
                egui::RichText::new("Columns")
                    .color(p.text_secondary)
                    .size(12.0),
            );
            for cfg in self.settings.columns.iter_mut() {
                let enabled = cfg.id != ColumnId::Name;
                if check_item(ui, &p, cfg.id.label(), cfg.visible, enabled) {
                    cfg.visible = !cfg.visible;
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

/// A menu row with a check mark in front when it is on: the same look for
/// every row (a tick or nothing, never a box), like a Windows 11 menu.
fn check_item(ui: &mut egui::Ui, p: &Palette, label: &str, checked: bool, enabled: bool) -> bool {
    let size = vec2(230.0, 30.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if enabled && resp.hovered() {
        ui.painter()
            .rect_filled(rect.shrink2(vec2(0.0, 1.0)), 4, p.row_hover);
    }
    let color = if enabled { p.text } else { p.text_tertiary };
    let painter = ui.painter();
    if checked {
        let c = pos2(rect.left() + 16.0, rect.center().y);
        let stroke = Stroke::new(1.6, color);
        painter.line_segment([c + vec2(-4.5, 0.5), c + vec2(-1.5, 3.5)], stroke);
        painter.line_segment([c + vec2(-1.5, 3.5), c + vec2(4.5, -3.5)], stroke);
    }
    painter.text(
        pos2(rect.left() + 34.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.0),
        color,
    );
    enabled && resp.clicked()
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
    // Tests (screenshots) show UTC so the output does not depend on the
    // time zone of the machine that runs them.
    #[cfg(not(test))]
    use chrono::Local;
    use chrono::TimeZone;
    #[cfg(test)]
    use chrono::Utc as Local;
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

    /// The ordering `column_header` relies on: a header-wide click area
    /// registered before a cell must not take the cell's click (the
    /// reverse order did, so sorting by a column did nothing).
    #[test]
    fn cell_click_wins_over_the_header_area_beneath_it() {
        let ctx = egui::Context::default();
        let header = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 30.0));
        let cell = Rect::from_min_size(pos2(200.0, 0.0), vec2(100.0, 30.0));
        let at = pos2(250.0, 15.0);
        let frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))),
                events,
                ..Default::default()
            };
            let mut clicked = (false, false);
            let mut output = ctx.run_ui(input, |ui| {
                let all = ui.interact(header, egui::Id::unique("all"), Sense::click());
                let c = ui.interact(cell, egui::Id::unique("cell"), Sense::click());
                clicked = (c.clicked(), all.clicked());
            });
            output.textures_delta.clear();
            clicked
        };
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![egui::Event::PointerMoved(at)]);
        frame(vec![egui::Event::PointerMoved(at)]);
        frame(vec![button(true)]);
        assert_eq!(frame(vec![button(false)]), (true, false));
    }

    fn defaults() -> Vec<(ColumnId, f32)> {
        vec![
            (ColumnId::Name, 320.0),
            (ColumnId::Folder, 0.0),
            (ColumnId::Size, 90.0),
            (ColumnId::Modified, 150.0),
            (ColumnId::Type, 110.0),
        ]
    }

    fn width_of(cols: &[Placed], id: ColumnId) -> Option<f32> {
        cols.iter().find(|c| c.id == id).map(|c| c.right - c.left)
    }

    #[test]
    fn a_wide_window_keeps_every_column_as_chosen() {
        let cols = place(&defaults(), 0.0, 1000.0);
        assert_eq!(cols.len(), 5);
        assert_eq!(width_of(&cols, ColumnId::Name), Some(320.0));
        assert_eq!(width_of(&cols, ColumnId::Size), Some(90.0));
        assert_eq!(width_of(&cols, ColumnId::Folder), Some(1000.0 - 670.0));
    }

    #[test]
    fn a_narrow_window_keeps_folder_readable_and_gives_way_elsewhere() {
        let cols = place(&defaults(), 0.0, 520.0);
        assert!(width_of(&cols, ColumnId::Folder).unwrap() >= MIN_FOLDER);
        assert!(width_of(&cols, ColumnId::Name).unwrap() >= useful_width(ColumnId::Name));
        // Name stops at its useful width; Folder gets what it does not need.
        assert_eq!(width_of(&cols, ColumnId::Name), Some(220.0));
        assert!(width_of(&cols, ColumnId::Folder).unwrap() > 200.0);
        // The rightmost column is left out first; the rest fill the width.
        assert_eq!(width_of(&cols, ColumnId::Type), None);
        assert!((cols.last().unwrap().right - 520.0).abs() < 0.01);
        for pair in cols.windows(2) {
            assert!((pair[0].right - pair[1].left).abs() < 0.01);
        }
    }

    #[test]
    fn name_is_never_left_out_and_the_chosen_order_is_kept() {
        for total in [200.0, 300.0, 400.0, 520.0, 700.0] {
            let cols = place(&defaults(), 0.0, total);
            assert_eq!(cols.first().map(|c| c.id), Some(ColumnId::Name), "{total}");
            let order: Vec<_> = cols.iter().map(|c| c.id).collect();
            let wanted: Vec<_> = defaults()
                .iter()
                .map(|c| c.0)
                .filter(|id| order.contains(id))
                .collect();
            assert_eq!(order, wanted, "{total}");
        }
    }

    #[test]
    fn dates_keep_a_width_that_shows_a_date() {
        let cols = place(&defaults(), 0.0, 640.0);
        if let Some(w) = width_of(&cols, ColumnId::Modified) {
            assert!(w >= 128.0, "{w}");
        }
    }

    #[test]
    fn without_a_folder_column_the_last_one_flexes() {
        let visible = vec![(ColumnId::Name, 320.0), (ColumnId::Size, 90.0)];
        let cols = place(&visible, 0.0, 700.0);
        assert_eq!(cols.len(), 2);
        assert!((cols[1].right - 700.0).abs() < 0.01);
    }

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
