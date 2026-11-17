//! What the results area says besides results: the status bar with its
//! state chip, the notice bars above the list, and the full-page states
//! (first start, indexing, no results), plus the search syntax panel.
//!
//! Everything here is drawn only when the window repaints, and only from
//! state the app already holds: no timers, no animation, no work per frame
//! beyond laying out a handful of short lines.

use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Pos2, Rect, Sense, Shape,
    Stroke, StrokeKind, pos2, vec2,
};
use ferrow_index::Index;

use super::{
    FerrowApp, NOTICE_GAP, NOTICE_HEIGHT, Page, ServiceProblem, State, ellipsized,
    filter_column_hint, fit, fmt_ms, group,
};
use crate::platform;
use crate::theme;

/// Colours for "all good", "needs a look" and "wrong": Windows 11 system
/// colours, one pair per theme.
#[derive(Clone, Copy)]
pub(super) struct Tones {
    pub ok: Color32,
    pub warn: Color32,
    pub error: Color32,
}

pub(super) fn tones(dark: bool) -> Tones {
    if dark {
        Tones {
            ok: Color32::from_rgb(0x6C, 0xCB, 0x5F),
            warn: Color32::from_rgb(0xFC, 0xB9, 0x3A),
            error: Color32::from_rgb(0xFF, 0x99, 0xA4),
        }
    } else {
        Tones {
            ok: Color32::from_rgb(0x0F, 0x7B, 0x0F),
            warn: Color32::from_rgb(0x9D, 0x5D, 0x00),
            error: Color32::from_rgb(0xC4, 0x2B, 0x1C),
        }
    }
}

// -------------------------------------------------------------------
// The state chip

/// Shape of the dot in the chip: the state is readable without colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dot {
    /// Filled: live from the Ferrow service.
    Live,
    /// Ring: this app's own index, not live.
    Own,
    /// Half filled: indexing.
    Busy,
}

struct Chip {
    dot: Dot,
    color: Color32,
    text: String,
    tooltip: String,
    /// Click opens Settings at "Service access".
    opens_settings: bool,
}

/// `1:05`, `12:30`, `1:02:03`.
pub(super) fn fmt_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The message under "Check the search text": the parser's text without the
/// technical position, first letter upper case.
pub(super) fn friendly_query_error(err: &str) -> String {
    let mut text = err.split(" at bytes").next().unwrap_or(err).to_owned();
    if let Some(first) = text.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    text
}

impl FerrowApp {
    /// The state chip: where results come from, or that an index is being
    /// built. `None` while nothing is loaded.
    fn chip(&self) -> Option<Chip> {
        let t = tones(self.palette.dark);
        let ready = matches!(self.state, State::Ready) && self.index.is_some();
        if self.scan_status.is_some() && (ready || matches!(self.state, State::Scanning(_))) {
            let info = self.scan_info.clone().unwrap_or_default();
            let text = if info.entries == 0 {
                "Indexing…".to_owned()
            } else {
                format!("Indexing · {} files", fit::compact_count(info.entries))
            };
            let mut tooltip = self.scan_status.clone().unwrap_or_default();
            if !info.location.is_empty() {
                tooltip.push_str(&format!("\nNow in {}", info.location));
            }
            tooltip.push_str(&format!("\n{} elapsed", fmt_elapsed(info.elapsed)));
            if ready {
                tooltip.push_str("\nSearch keeps using the current index until this finishes.");
            }
            return Some(Chip {
                dot: Dot::Busy,
                color: self.palette.accent_text,
                text,
                tooltip,
                opens_settings: false,
            });
        }
        if !ready {
            return None;
        }
        if let Some(entries) = self.service_entries {
            return Some(Chip {
                dot: Dot::Live,
                color: t.ok,
                text: format!(
                    "Live · Ferrow service · {} files",
                    fit::compact_count(entries)
                ),
                tooltip: format!(
                    "{}\nResults come from the Ferrow system service and stay current by themselves.",
                    self.index_source
                ),
                opens_settings: false,
            });
        }
        let (text, reason) = match &self.service_problem {
            None => (
                "Own index · not live",
                "The Ferrow service isn't installed or isn't running, so this app searches its own index of the folders you chose. Click for Settings → Service access."
                    .to_owned(),
            ),
            Some(ServiceProblem::Refused) => (
                "Own index · account not allowed",
                "The Ferrow service doesn't allow this account — an administrator can allow it in Settings. Click for Settings → Service access."
                    .to_owned(),
            ),
            Some(ServiceProblem::Lost(error)) => (
                "Own index · service stopped",
                format!(
                    "The Ferrow service stopped answering ({error}). Using this app's own index until it's back."
                ),
            ),
        };
        Some(Chip {
            dot: Dot::Own,
            color: t.warn,
            text: text.to_owned(),
            tooltip: reason,
            opens_settings: true,
        })
    }

    /// Open Settings at a card (the page scrolls to it when it can).
    pub(super) fn open_settings_at(&mut self, card: &'static str) {
        self.page = Page::Settings;
        self.settings_scroll_to = Some(card);
        self.focus_search = false;
    }

    pub(super) fn status_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        let rect = ui.max_rect();
        let font = FontId::proportional(12.0);
        let cy = rect.center().y;
        let mut x = rect.left() + 2.0;

        if let Some(chip) = self.chip() {
            let galley = ui.painter().layout_no_wrap(
                chip.text.clone(),
                FontId::new(12.0, FontFamily::Name(theme::SEMIBOLD.into())),
                p.text,
            );
            let width = (galley.size().x + 34.0).min(rect.width());
            let chip_rect =
                Rect::from_min_size(pos2(x, rect.top() + 3.0), vec2(width, rect.height() - 6.0));
            let sense = if chip.opens_settings {
                Sense::click()
            } else {
                Sense::hover()
            };
            let resp = ui.interact(chip_rect, egui::Id::unique("state-chip"), sense);
            if chip.opens_settings && resp.hovered() {
                ui.painter()
                    .rect_filled(chip_rect, CornerRadius::same(4), p.row_hover);
            }
            paint_dot(
                ui.painter(),
                pos2(chip_rect.left() + 12.0, cy),
                chip.dot,
                chip.color,
            );
            ui.painter().galley(
                pos2(chip_rect.left() + 24.0, cy - galley.size().y / 2.0),
                galley,
                p.text,
            );
            let clicked = chip.opens_settings && resp.clicked();
            let resp = if chip.opens_settings {
                resp.on_hover_cursor(CursorIcon::PointingHand)
            } else {
                resp
            };
            resp.on_hover_text(chip.tooltip);
            if clicked {
                self.open_settings_at("Service access");
            }
            x = chip_rect.right() + 10.0;
        }

        // The result count, then hints on the right.
        let (count, timing, count_color) = self.count_text();
        if !count.is_empty() {
            let galley = ui
                .painter()
                .layout_no_wrap(count, font.clone(), count_color);
            let w = galley.size().x;
            let r = Rect::from_min_size(pos2(x, rect.top()), vec2(w, rect.height()));
            ui.painter()
                .galley(pos2(x, cy - galley.size().y / 2.0), galley, count_color);
            ui.interact(r, egui::Id::unique("count"), Sense::hover())
                .on_hover_text(timing);
            x += w + 14.0;
        }
        let hint = self.status_hint();
        let room = rect.right() - 2.0 - x;
        if let Some(hint) = hint
            && room > 80.0
        {
            let galley = ellipsized(ui, hint.clone(), font, p.text_tertiary, room);
            let r = Rect::from_min_size(
                pos2(rect.right() - 2.0 - galley.size().x, rect.top()),
                vec2(galley.size().x, rect.height()),
            );
            ui.painter().galley(
                pos2(r.left(), cy - galley.size().y / 2.0),
                galley.clone(),
                p.text_tertiary,
            );
            if galley.elided {
                ui.interact(r, egui::Id::unique("hint"), Sense::hover())
                    .on_hover_text(hint);
            }
        }
    }

    /// `("82 results", "Found in 0.3 ms", colour)`; empty text when there is
    /// none. A search text that does not parse says so here, in the error
    /// colour: the list keeps its place (dimmed) instead of jumping.
    fn count_text(&self) -> (String, String, Color32) {
        let p = self.palette;
        let (State::Ready, Some(index)) = (&self.state, &self.index) else {
            return (String::new(), String::new(), p.text_secondary);
        };
        if let Some((err, _)) = &self.query_error {
            return (
                format!("Check the search text · {}", friendly_query_error(err)),
                err.clone(),
                tones(p.dark).error,
            );
        }
        let (text, tooltip) = self.result_count(index);
        (text, tooltip, p.text_secondary)
    }

    fn result_count(&self, index: &Index) -> (String, String) {
        if self.query.trim().is_empty() {
            // A live service says its size in the chip.
            return match self.service_entries {
                Some(_) => Default::default(),
                None => (
                    format!("{} files", group(self.object_count(index))),
                    "Entries in this app's own index".to_owned(),
                ),
            };
        }
        let rows = self.row_count();
        let text = if self.complete {
            format!(
                "{} {}",
                group(rows),
                if rows == 1 { "result" } else { "results" }
            )
        } else {
            format!("{}+ results", group(rows))
        };
        (text, format!("Found in {}", fmt_ms(self.search_time)))
    }

    /// What the right side of the status bar says: the sort being prepared,
    /// or a filter whose column is missing.
    fn status_hint(&self) -> Option<String> {
        let (State::Ready, Some(index)) = (&self.state, &self.index) else {
            return None;
        };
        if self.query_error.is_some() {
            return None;
        }
        if self.sort_pending {
            Some(format!("Preparing sort by {}…", self.settings.sort.label()))
        } else if let Some(hint) = filter_column_hint(&self.query, index) {
            Some(hint)
        } else if self.columns_need_rebuild {
            Some("Rebuild the index to fill the new columns".to_owned())
        } else {
            None
        }
    }

    // ---------------------------------------------------------------
    // Notice bars

    /// Bars to show above the list, most important first: (colour, text,
    /// hover text).
    pub(super) fn notices(&self) -> Vec<Notice> {
        let t = tones(self.palette.dark);
        let mut out = Vec::new();
        if let Some(ServiceProblem::Lost(error)) = &self.service_problem {
            out.push(Notice {
                color: t.warn,
                text: "The Ferrow service stopped answering. Using this app's own index until it's back."
                    .to_owned(),
                tooltip: error.clone(),
            });
        }
        out
    }

    pub(super) fn draw_notices(
        &self,
        ui: &mut egui::Ui,
        notices: &[Notice],
        top_left: Pos2,
        width: f32,
    ) {
        let p = self.palette;
        for (i, n) in notices.iter().enumerate() {
            let rect = Rect::from_min_size(
                top_left + vec2(0.0, i as f32 * (NOTICE_HEIGHT + NOTICE_GAP)),
                vec2(width, NOTICE_HEIGHT),
            );
            let painter = ui.painter();
            painter.rect(
                rect,
                CornerRadius::same(6),
                n.color.gamma_multiply(if p.dark { 0.16 } else { 0.12 }),
                Stroke::new(1.0, n.color.gamma_multiply(0.45)),
                StrokeKind::Inside,
            );
            painter.circle_filled(pos2(rect.left() + 14.0, rect.center().y), 3.5, n.color);
            let galley = ellipsized(
                ui,
                n.text.clone(),
                FontId::proportional(13.0),
                p.text,
                rect.width() - 36.0,
            );
            ui.painter().galley(
                pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0),
                galley,
                p.text,
            );
            ui.interact(rect, egui::Id::unique(("notice", i)), Sense::hover())
                .on_hover_text(&n.tooltip);
        }
    }

    // ---------------------------------------------------------------
    // Full-page states

    /// Indexing for the first time: what is happening, not a blank page.
    pub(super) fn scanning_page(&self, ui: &mut egui::Ui, rect: Rect, sources: &str) {
        let p = self.palette;
        let info = self.scan_info.clone().unwrap_or_default();
        let semibold = |size| FontId::new(size, FontFamily::Name(theme::SEMIBOLD.into()));
        let mut lines = vec![Line::new(
            "Indexing your files",
            semibold(20.0),
            p.text,
            14.0,
        )];
        if info.entries == 0 {
            lines.push(Line::new(
                "Starting…",
                FontId::proportional(13.0),
                p.text_secondary,
                0.0,
            ));
        } else {
            lines.push(Line::new(
                format!("{} entries found", group(info.entries as usize)),
                semibold(28.0),
                p.accent_text,
                10.0,
            ));
            if !info.location.is_empty() {
                lines.push(Line::path(
                    &info.location,
                    FontId::proportional(13.0),
                    p.text_secondary,
                    4.0,
                ));
            }
            lines.push(Line::new(
                format!("{} elapsed", fmt_elapsed(info.elapsed)),
                FontId::proportional(13.0),
                p.text_tertiary,
                14.0,
            ));
        }
        lines.push(Line::path(
            &format!(
                "Locations: {}",
                sources
                    .trim_start_matches("Indexing ")
                    .trim_end_matches('…')
            ),
            FontId::proportional(12.0),
            p.text_tertiary,
            4.0,
        ));
        lines.push(Line::new(
            "Search opens as soon as this finishes.",
            FontId::proportional(12.0),
            p.text_tertiary,
            0.0,
        ));
        stack(ui, rect, lines, 0.0);
    }

    /// First start: choose what to search.
    pub(super) fn first_start(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let p = self.palette;
        let t = tones(p.dark);
        let refused = matches!(self.service_problem, Some(ServiceProblem::Refused));
        let mut lines = vec![
            Line::new(
                "Choose what Ferrow searches",
                FontId::new(20.0, FontFamily::Name(theme::SEMIBOLD.into())),
                p.text,
                6.0,
            ),
            Line::new(
                "Ferrow keeps a private index, so results appear as you type.",
                FontId::proportional(13.0),
                p.text_secondary,
                if refused { 10.0 } else { 16.0 },
            ),
        ];
        if refused {
            lines.push(Line::new(
                "The Ferrow service is installed, but doesn't allow this account.",
                FontId::proportional(13.0),
                t.warn,
                16.0,
            ));
        }
        let windows = cfg!(windows);
        let buttons = 1 + usize::from(windows) + usize::from(refused);
        let below = 44.0 + 40.0 * (buttons as f32 - 1.0) + 8.0 + 20.0;
        let mut y = stack(ui, rect, lines, below);
        let cx = rect.center().x;
        let wide = 312.0_f32.min(rect.width() - 32.0);
        let mut action = None;
        let primary = if windows {
            "All drives (recommended)"
        } else {
            "Your home folder (recommended)"
        };
        if self.big_button(
            ui,
            Rect::from_center_size(pos2(cx, y + 18.0), vec2(wide, 36.0)),
            primary,
            true,
        ) {
            action = Some(FirstStart::Primary);
        }
        y += 44.0;
        let secondary: Vec<(&str, FirstStart)> = if windows {
            vec![
                ("Only my home folder", FirstStart::Home),
                ("Choose folders…", FirstStart::Choose),
            ]
        } else {
            vec![("Choose folders…", FirstStart::Choose)]
        };
        // Two secondary buttons share a row when they fit.
        let half = (wide - 8.0) / 2.0;
        let row_w = half * secondary.len() as f32 + 8.0 * (secondary.len() as f32 - 1.0);
        if row_w <= rect.width() - 24.0 {
            let mut left = cx - row_w / 2.0;
            for (label, what) in &secondary {
                let r = Rect::from_min_size(pos2(left, y), vec2(half, 32.0));
                if self.big_button(ui, r, label, false) {
                    action = Some(*what);
                }
                left += half + 8.0;
            }
            y += 40.0;
        } else {
            for (label, what) in &secondary {
                let r = Rect::from_center_size(pos2(cx, y + 16.0), vec2(wide, 32.0));
                if self.big_button(ui, r, label, false) {
                    action = Some(*what);
                }
                y += 40.0;
            }
        }
        if refused {
            let r = Rect::from_center_size(pos2(cx, y + 16.0), vec2(wide, 32.0));
            if self.big_button(ui, r, "Open Settings", false) {
                action = Some(FirstStart::Service);
            }
            y += 40.0;
        }
        let hint = ui.painter().layout_no_wrap(
            "You can change this any time in Settings.".to_owned(),
            FontId::proportional(12.0),
            p.text_tertiary,
        );
        ui.painter().galley(
            pos2(cx - hint.size().x / 2.0, y + 4.0),
            hint,
            p.text_tertiary,
        );
        match action {
            Some(FirstStart::Primary) if windows => {
                self.settings.windows.all_fixed_drives = true;
                self.save_settings();
                self.rebuild_index();
            }
            Some(FirstStart::Primary | FirstStart::Home) => {
                if let Some(home) = crate::engine::home_dir() {
                    self.index_home(home);
                }
            }
            Some(FirstStart::Choose) => self.open_settings_at("Index"),
            Some(FirstStart::Service) => self.open_settings_at("Service access"),
            None => {}
        }
    }

    /// A button for the full-page states; returns whether it was clicked.
    fn big_button(&self, ui: &mut egui::Ui, rect: Rect, label: &str, primary: bool) -> bool {
        let p = self.palette;
        let resp = ui.interact(
            rect,
            egui::Id::unique(("big-button", label)),
            Sense::click(),
        );
        let (fill, stroke, text) = if primary {
            let fill = if resp.is_pointer_button_down_on() {
                p.accent_pressed
            } else if resp.hovered() {
                p.accent_hover
            } else {
                p.accent
            };
            (fill, Stroke::NONE, Color32::WHITE)
        } else {
            let fill = if resp.is_pointer_button_down_on() {
                p.control_pressed
            } else if resp.hovered() {
                p.control_hover
            } else {
                p.control
            };
            (fill, Stroke::new(1.0, p.control_stroke), p.text)
        };
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(4),
            fill,
            stroke,
            StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::new(14.0, FontFamily::Name(theme::SEMIBOLD.into())),
            text,
        );
        resp.on_hover_cursor(CursorIcon::Default).clicked()
    }

    /// The search found nothing: what was searched and what to try.
    pub(super) fn no_results(&self, ui: &mut egui::Ui, rect: Rect, index: &Index) {
        let p = self.palette;
        let width = rect.width() - 48.0;
        let semibold = FontId::new(18.0, FontFamily::Name(theme::SEMIBOLD.into()));
        let title = format!("No results for “{}”", self.query.trim());
        let scope = if self.service_entries.is_some() {
            "Searched every drive · Ferrow service".to_owned()
        } else {
            let places = self.settings.sources.len()
                + usize::from(cfg!(windows) && self.settings.windows.all_fixed_drives);
            let places = places.max(1);
            format!(
                "Searched {} files in {places} {}",
                group(index.len()),
                if places == 1 { "location" } else { "locations" }
            )
        };
        let filters = fit::query_filters(&self.query);
        let mut lines = vec![
            Line::new(title, semibold, p.text, 8.0),
            Line::new(scope, FontId::proportional(13.0), p.text_secondary, 4.0),
        ];
        if !filters.is_empty() {
            lines.push(Line::new(
                format!("Filters: {}", filters.join(", ")),
                FontId::proportional(13.0),
                p.accent_text,
                10.0,
            ));
        }
        lines.push(Line::new(
            if filters.is_empty() {
                "Check the spelling, or try fewer or shorter words."
            } else {
                "Check the spelling, or remove a filter."
            },
            FontId::proportional(13.0),
            p.text_tertiary,
            0.0,
        ));
        let _ = width;
        stack(ui, rect, lines, 0.0);
    }
}

#[derive(Clone, Copy)]
enum FirstStart {
    /// All drives (Windows) or the home folder.
    Primary,
    Home,
    Choose,
    Service,
}

/// One bar above the list.
pub(super) struct Notice {
    color: Color32,
    text: String,
    tooltip: String,
}

/// One centred line of a full-page state.
struct Line {
    text: String,
    font: FontId,
    color: Color32,
    /// Space below the line.
    gap: f32,
    /// A path: a long one is cut in the middle, not at the end.
    path: bool,
}

impl Line {
    fn new(text: impl Into<String>, font: FontId, color: Color32, gap: f32) -> Line {
        Line {
            text: text.into(),
            font,
            color,
            gap,
            path: false,
        }
    }

    fn path(text: &str, font: FontId, color: Color32, gap: f32) -> Line {
        Line {
            path: true,
            ..Line::new(text, font, color, gap)
        }
    }
}

/// Paint `lines` centred in `rect` (with `extra` room reserved below them)
/// and return the y where the reserved room starts.
fn stack(ui: &egui::Ui, rect: Rect, lines: Vec<Line>, extra: f32) -> f32 {
    let width = (rect.width() - 32.0).max(40.0);
    let galleys: Vec<(Arc<egui::Galley>, f32)> = lines
        .into_iter()
        .map(|l| {
            let text = if l.path {
                let ctx = ui.ctx().clone();
                fit::fit_path(&l.text, width, &|s| {
                    ctx.fonts_mut(|f| {
                        f.layout_no_wrap(s.to_owned(), l.font.clone(), Color32::WHITE)
                            .size()
                            .x
                    })
                })
            } else {
                l.text
            };
            (ellipsized(ui, text, l.font, l.color, width), l.gap)
        })
        .collect();
    let total: f32 = galleys.iter().map(|(g, gap)| g.size().y + gap).sum::<f32>() + extra;
    let mut y = rect.center().y - total / 2.0;
    for (galley, gap) in galleys {
        let x = rect.center().x - galley.size().x / 2.0;
        let h = galley.size().y;
        ui.painter().galley(pos2(x, y), galley, Color32::WHITE);
        y += h + gap;
    }
    y
}

fn paint_dot(painter: &egui::Painter, c: Pos2, dot: Dot, color: Color32) {
    match dot {
        Dot::Live => {
            painter.circle_filled(c, 4.5, color);
        }
        Dot::Own => {
            painter.circle_stroke(c, 3.75, Stroke::new(1.6, color));
        }
        Dot::Busy => {
            painter.circle_stroke(c, 3.75, Stroke::new(1.6, color));
            // The left half filled.
            let pts: Vec<Pos2> = (0..=8)
                .map(|i| {
                    let a = std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * i as f32 / 8.0;
                    c + vec2(a.cos(), a.sin()) * 3.0
                })
                .collect();
            painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
        }
    }
}

// -------------------------------------------------------------------
// Search syntax

/// Search syntax by group: (group, rows of (example, meaning)).
const SYNTAX_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Combine",
        &[
            ("report 2024", "Both words (AND)"),
            ("jpg | png", "Either word (OR)"),
            ("!tmp", "Without this word (NOT)"),
            ("<a | b> c", "Group terms"),
            ("\"two words\"", "Exact phrase"),
        ],
    ),
    (
        "Match",
        &[
            ("*.log", "Wildcards: * any run, ? one character"),
            ("report*2024*", "Starts with report, contains 2024"),
            ("case:Readme", "Case-sensitive"),
            ("wholeword:log", "Whole words only"),
            ("regex:^\\d+\\.txt$", "Regular expression"),
            ("path:docs", "Match the full path, not just the name"),
        ],
    ),
    (
        "Filters",
        &[
            ("ext:log;txt", "File extensions"),
            (
                "audio:beatles",
                "File types: audio: video: pic: doc: archive: exe:",
            ),
            ("file:  folder:", "Only files, or only folders"),
            ("parent:C:\\logs", "Direct children of a folder"),
            ("depth:2", "Folder depth"),
            ("size:>10mb", "Size: >, >=, <, <=, or 1mb..5mb"),
            ("dm:today", "Modified today (dc: created, da: accessed)"),
            ("dm:last10minutes", "Last 10 minutes (hours, days, weeks)"),
            ("dm:2026-01-01..2026-01-31", "Modified in a date range"),
            ("attrib:H", "Attributes: R H S A C E and more"),
        ],
    ),
];

/// Width of one syntax column.
const SYNTAX_COLUMN: f32 = 420.0;

impl FerrowApp {
    /// The search syntax panel: right under the search box (never over it),
    /// inside the window, scrolling when the window is short, two columns
    /// when it is wide.
    pub(super) fn syntax_help(&mut self, ui: &mut egui::Ui, search: Rect, button: Rect) {
        let p = self.palette;
        let ctx = ui.ctx().clone();
        let screen = ctx.content_rect();
        let two = screen.width() >= 2.0 * SYNTAX_COLUMN + 56.0;
        let width = if two {
            2.0 * SYNTAX_COLUMN + 40.0
        } else {
            (screen.width() - 16.0).min(SYNTAX_COLUMN + 120.0)
        };
        let left = (search.right() - width)
            .clamp(screen.left() + 8.0, (screen.right() - width - 8.0).max(8.0));
        let top = search.bottom() + 6.0;
        let max_h = (screen.bottom() - top - 8.0).max(120.0);
        let area = egui::Area::new(egui::Id::unique("syntax-help-panel"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(left, top))
            .constrain(true);
        let shown = area.show(&ctx, |ui| {
            egui::Frame::popup(&ctx.global_style())
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(width - 24.0);
                    ui.label(
                        egui::RichText::new("Search syntax")
                            .font(FontId::new(14.0, FontFamily::Name(theme::SEMIBOLD.into()))),
                    );
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .max_height(max_h - 56.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            let columns: Vec<Vec<usize>> = if two {
                                vec![vec![0, 1], vec![2]]
                            } else {
                                vec![vec![0, 1, 2]]
                            };
                            ui.horizontal_top(|ui| {
                                for groups in &columns {
                                    ui.vertical(|ui| {
                                        ui.set_width(if two {
                                            SYNTAX_COLUMN
                                        } else {
                                            width - 40.0
                                        });
                                        for &g in groups {
                                            syntax_group(ui, &p, SYNTAX_GROUPS[g]);
                                        }
                                    });
                                }
                            });
                        });
                });
        });
        // Closes on Escape, or a click anywhere else.
        let area_rect = shown.response.rect;
        let escape = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        let outside_click = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|pos| !area_rect.contains(pos) && !button.contains(pos))
        });
        if escape || outside_click {
            self.syntax_open = false;
            if escape {
                self.focus_search = true;
            }
        }
        let _ = platform::CUSTOM_TITLE_ROW;
    }
}

fn syntax_group(ui: &mut egui::Ui, p: &theme::Palette, (title, rows): (&str, &[(&str, &str)])) {
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(title)
            .font(FontId::new(12.0, FontFamily::Name(theme::SEMIBOLD.into())))
            .color(p.text_tertiary),
    );
    ui.add_space(2.0);
    // The examples column is as wide as the group's longest example.
    let example_w = rows
        .iter()
        .map(|(e, _)| {
            ui.ctx().fonts_mut(|f| {
                f.layout_no_wrap((*e).to_owned(), FontId::monospace(13.0), Color32::WHITE)
                    .size()
                    .x
            })
        })
        .fold(0.0_f32, f32::max)
        + 2.0;
    for (example, meaning) in rows {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.allocate_ui(vec2(example_w, 18.0), |ui| {
                ui.set_min_width(example_w);
                ui.label(
                    egui::RichText::new(*example)
                        .monospace()
                        .color(p.accent_text),
                );
            });
            ui.add(egui::Label::new(egui::RichText::new(*meaning).color(p.text_secondary)).wrap());
        });
    }
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use super::super::State;
    use super::*;
    use crate::engine::{Engine, Event, Hits, ServiceGap};
    use crate::settings::Settings;
    use crate::theme::Palette;
    use ferrow_index::flags;

    #[test]
    fn elapsed_time_reads_like_a_clock() {
        assert_eq!(fmt_elapsed(Duration::from_secs(0)), "0:00");
        assert_eq!(fmt_elapsed(Duration::from_secs(65)), "1:05");
        assert_eq!(fmt_elapsed(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn the_parser_position_is_not_shown() {
        assert_eq!(
            friendly_query_error("unclosed group at bytes 0..1"),
            "Unclosed group"
        );
        assert_eq!(friendly_query_error("expected a value"), "Expected a value");
        assert_eq!(friendly_query_error(""), "");
    }

    #[test]
    fn the_syntax_help_has_every_filter_the_parser_knows() {
        let all: String = SYNTAX_GROUPS
            .iter()
            .flat_map(|(_, rows)| rows.iter().map(|(e, _)| *e))
            .collect::<Vec<_>>()
            .join(" ");
        for needle in [
            "ext:",
            "parent:",
            "depth:",
            "size:",
            "dm:",
            "attrib:",
            "regex:",
            "wholeword:",
            "case:",
            "path:",
            "file:",
            "folder:",
        ] {
            assert!(all.contains(needle), "{needle} is missing from the help");
        }
    }

    // ---- States driven through the real event handling (isolated engine).

    fn test_app() -> (FerrowApp, std::sync::mpsc::Sender<Event>) {
        let (engine, events) = Engine::isolated(None);
        let settings = Settings::default();
        let app = FerrowApp::assemble(
            engine,
            settings,
            Palette::new(true, None),
            false,
            true,
            None,
            false,
        );
        (app, events)
    }

    fn sample_index() -> Arc<Index> {
        let b = ferrow_index::IndexBuilder::new();
        let root = b.add_root(b"/r", flags::DIR);
        for name in ["alpha.txt", "beta.rs", "gamma"] {
            b.add(root, name.as_bytes(), 0);
        }
        Arc::new(b.freeze())
    }

    /// Hand the app the events and let it handle them; the isolated engine
    /// delivers in order.
    fn feed(app: &mut FerrowApp, tx: &std::sync::mpsc::Sender<Event>, events: Vec<Event>) {
        for e in events {
            tx.send(e).unwrap();
        }
        // The channel is read by `poll`, which `drain_events` loops on.
        app.drain_events();
    }

    fn loaded(index: &Arc<Index>) -> Event {
        Event::Loaded {
            index: index.clone(),
            elapsed: Duration::from_millis(5),
            source: "test".into(),
        }
    }

    fn results(generation: u64) -> Event {
        Event::Results {
            generation,
            hits: Hits::All,
            complete: true,
            elapsed: Duration::from_micros(300),
            sort_pending: false,
            sort_unavailable: false,
        }
    }

    /// A search text that does not parse must not blank the list: the last
    /// results stay (the page dims them), the message goes to the status
    /// bar. On the base commit this state showed a full-page "Check the
    /// search text" instead.
    #[test]
    fn a_search_text_error_keeps_the_last_results() {
        let (mut app, tx) = test_app();
        let index = sample_index();
        feed(&mut app, &tx, vec![loaded(&index), results(1)]);
        assert_eq!(app.row_count(), 4);
        feed(
            &mut app,
            &tx,
            vec![Event::QueryError {
                generation: 2,
                message: "unclosed group at bytes 0..1".into(),
                span: 0..1,
            }],
        );
        assert!(app.query_error.is_some());
        assert_eq!(app.row_count(), 4, "the list keeps its rows");
        let (text, _, _) = app.count_text();
        assert_eq!(text, "Check the search text · Unclosed group");
        // The next good search clears the message.
        feed(&mut app, &tx, vec![results(3)]);
        assert!(app.query_error.is_none());
        assert_eq!(app.count_text().0, "4 files");
    }

    /// A lost service keeps the rows on screen and says so in a notice bar;
    /// it never becomes the full-page "Something went wrong".
    #[test]
    fn a_lost_service_keeps_the_results_and_shows_a_notice() {
        let (mut app, tx) = test_app();
        feed(
            &mut app,
            &tx,
            vec![Event::Remote {
                entries: 18_422_907,
                report: "NTFS journal".into(),
            }],
        );
        let row = |path: &str| ferrow_ipc::FileRow {
            path: path.as_bytes().to_vec(),
            ..Default::default()
        };
        feed(
            &mut app,
            &tx,
            vec![Event::Results {
                generation: 1,
                hits: crate::engine::service_hits_for_test(
                    1,
                    2,
                    vec![row("/a/x.txt"), row("/a/y.txt")],
                ),
                complete: true,
                elapsed: Duration::from_micros(900),
                sort_pending: false,
                sort_unavailable: false,
            }],
        );
        assert_eq!(app.row_count(), 2);
        assert!(
            app.chip()
                .unwrap()
                .text
                .starts_with("Live · Ferrow service")
        );
        feed(
            &mut app,
            &tx,
            vec![Event::ServiceLost {
                error: "the pipe has been ended. (os error 109)".into(),
            }],
        );
        assert!(matches!(app.state, State::Ready), "not a failure page");
        assert_eq!(app.row_count(), 2, "the rows stay");
        let notices = app.notices();
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0]
                .text
                .starts_with("The Ferrow service stopped answering.")
        );
        assert!(
            !notices[0].text.contains("os error"),
            "the raw error is only in the tooltip"
        );
        assert!(notices[0].tooltip.contains("os error 109"));
        assert_eq!(app.chip().unwrap().text, "Own index · service stopped");
        // It is back: the notice goes.
        feed(
            &mut app,
            &tx,
            vec![Event::Remote {
                entries: 18_422_907,
                report: "NTFS journal".into(),
            }],
        );
        assert!(app.notices().is_empty());
    }

    /// A refused account is its own state in the app, shown in the chip.
    #[test]
    fn a_refused_account_is_its_own_chip_state() {
        let (mut app, tx) = test_app();
        let index = sample_index();
        feed(
            &mut app,
            &tx,
            vec![Event::ServiceGap(ServiceGap::Refused), loaded(&index)],
        );
        let chip = app.chip().unwrap();
        assert_eq!(chip.text, "Own index · account not allowed");
        assert!(chip.tooltip.contains("doesn't allow this account"));
        assert!(chip.opens_settings);
        // Not installed or not running is a different text.
        feed(
            &mut app,
            &tx,
            vec![Event::ServiceGap(ServiceGap::NotRunning)],
        );
        assert_eq!(app.chip().unwrap().text, "Own index · not live");
    }

    /// Progress reports only matter while a scan runs: a late report after the
    /// index is loaded must not bring the indexing state back.
    #[test]
    fn a_late_progress_report_is_ignored_and_a_live_one_shows_in_the_chip() {
        let (mut app, tx) = test_app();
        let index = sample_index();
        let progress = |entries| Event::ScanProgress {
            entries,
            location: r"D:\x".into(),
            elapsed: Duration::from_secs(3),
        };
        feed(&mut app, &tx, vec![loaded(&index), progress(5)]);
        assert!(app.scan_info.is_none());
        assert_eq!(app.chip().unwrap().text, "Own index · not live");
        feed(
            &mut app,
            &tx,
            vec![
                Event::Scanning(r"Indexing D:\…".into()),
                progress(1_204_331),
            ],
        );
        assert_eq!(app.chip().unwrap().text, "Indexing · 1.2 M files");
        feed(&mut app, &tx, vec![loaded(&index)]);
        assert!(app.scan_info.is_none());
    }
}
