//! The main window: search box, virtual result list, status bar.
// The custom title row (caption buttons, app mark) is Windows-only.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Key, Modifiers, Pos2,
    Rect, Sense, Shape, Stroke, StrokeKind, pos2, text::LayoutJob, vec2,
};
use ferrow_index::{Index, flags};

use crate::engine::{Engine, Event, Hits, home_dir};
use crate::logo::TitleLogo;
use crate::settings::Settings;

#[path = "settings_page.rs"]
mod settings_page;
use crate::platform;
use crate::theme::{self, Palette};

const ROW_HEIGHT: f32 = 32.0;
const HEADER_HEIGHT: f32 = 32.0;
const SCROLLBAR_WIDTH: f32 = 12.0;
/// Title row height; matches the Windows 11 tall title bar.
const TITLE_HEIGHT: f32 = 40.0;
/// Settings (gear) button in the title row.
const GEAR_WIDTH: f32 = 40.0;
/// Room for the system minimise / maximise / close buttons.
/// Width of one caption button (Windows 11 metrics).
const CAPTION_BUTTON_WIDTH: f32 = 46.0;
const CAPTION_BUTTONS_WIDTH: f32 = CAPTION_BUTTON_WIDTH * 3.0;
const STATUS_HEIGHT: f32 = 28.0;

enum State {
    Loading,
    NoIndex { path: Option<PathBuf> },
    Scanning(String),
    Failed(String),
    Ready,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Results,
    Settings,
}

/// Which folder list a folder dialog adds to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PickTarget {
    Root,
    Exclude,
}

pub struct FerrowApp {
    engine: Engine,
    state: State,
    index: Option<Arc<Index>>,
    index_source: String,
    load_time: Duration,
    query: String,
    sent_query: Option<String>,
    shown_generation: u64,
    hits: Option<Hits>,
    complete: bool,
    search_time: Duration,
    query_error: Option<String>,
    /// Top visible row, fractional for smooth scrolling.
    scroll: f64,
    selected: Option<usize>,
    /// Keep the selection in view after keyboard moves.
    follow_selection: bool,
    name_width: f32,
    palette: Palette,
    backdrop: bool,
    dark: bool,
    accent: Option<[u8; 3]>,
    focus_search: bool,
    title_logo: TitleLogo,
    /// Running inside a Remote Desktop session.
    remote: bool,
    page: Page,
    settings: Settings,
    folder_pick: Option<(PickTarget, std::sync::mpsc::Receiver<Option<PathBuf>>)>,
    /// Set while a rebuild runs; search keeps using the old index.
    scan_status: Option<String>,
    settings_error: Option<String>,
    /// Drives for the "Add drive" menu, loaded off the UI thread.
    drives: Option<Vec<platform::DriveInfo>>,
    drives_rx: Option<std::sync::mpsc::Receiver<Vec<platform::DriveInfo>>>,
    /// Text of the "Add network location" field while it is open.
    network_input: Option<String>,
    /// The on-start automatic rebuild already ran in this session.
    auto_started: bool,
}

impl FerrowApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<FerrowApp, String> {
        let ctx = cc.egui_ctx.clone();
        theme::install_fonts(&ctx)?;
        let settings = Settings::load();
        settings_page::apply_theme_choice(&ctx, settings.theme);
        let dark = ctx.theme() == egui::Theme::Dark;
        let accent = platform::accent_color();
        let palette = Palette::new(dark, accent);
        theme::apply_style(&ctx, &palette);
        let backdrop = platform::apply_backdrop(cc, dark);
        let frame_wake = ctx.clone();
        platform::install_frame(cc, dark, move || frame_wake.request_repaint());
        let wake = ctx.clone();
        let engine = Engine::start(move || wake.request_repaint());
        Ok(FerrowApp {
            engine,
            state: State::Loading,
            index: None,
            index_source: String::new(),
            load_time: Duration::ZERO,
            query: String::new(),
            sent_query: None,
            shown_generation: 0,
            hits: None,
            complete: true,
            search_time: Duration::ZERO,
            query_error: None,
            scroll: 0.0,
            selected: None,
            follow_selection: false,
            name_width: 0.0,
            palette,
            backdrop,
            dark,
            accent,
            focus_search: true,
            title_logo: TitleLogo::new(),
            remote: platform::is_remote_session(),
            page: Page::Results,
            settings,
            folder_pick: None,
            scan_status: None,
            settings_error: None,
            drives: None,
            drives_rx: None,
            network_input: None,
            auto_started: false,
        })
    }

    fn drain_events(&mut self) {
        while let Some(event) = self.engine.poll() {
            match event {
                Event::Loaded {
                    index,
                    elapsed,
                    source,
                } => {
                    self.index = Some(index);
                    self.scan_status = None;
                    self.load_time = elapsed;
                    self.index_source = source;
                    self.state = State::Ready;
                    self.sent_query = None; // re-run the current query
                }
                Event::NoIndex { path } => self.state = State::NoIndex { path },
                Event::Failed(msg) => {
                    self.scan_status = None;
                    if self.index.is_some() {
                        // Keep searching the current index; report the error.
                        self.settings_error = Some(msg);
                    } else {
                        self.state = State::Failed(msg);
                    }
                }
                Event::Scanning(msg) => {
                    if self.index.is_none() {
                        self.state = State::Scanning(msg.clone());
                    }
                    self.scan_status = Some(msg);
                }
                Event::Results {
                    generation,
                    hits,
                    complete,
                    elapsed,
                } => {
                    if generation < self.shown_generation {
                        continue;
                    }
                    if generation > self.shown_generation {
                        // A new query: start at the top.
                        self.scroll = 0.0;
                        self.selected = None;
                    }
                    self.shown_generation = generation;
                    self.hits = Some(hits);
                    self.complete = complete;
                    self.search_time = elapsed;
                    self.query_error = None;
                }
                Event::QueryError {
                    generation,
                    message,
                } => {
                    if generation >= self.shown_generation {
                        self.shown_generation = generation;
                        self.query_error = Some(message);
                    }
                }
            }
        }
    }

    /// Automatic rebuilds, when the user turned them on. A due rebuild is
    /// scheduled with one delayed repaint, never with a polling timer.
    fn auto_rebuild(&mut self, ctx: &egui::Context) {
        use crate::settings::AutoRebuild;
        if !matches!(self.state, State::Ready | State::NoIndex { .. })
            || self.scan_status.is_some()
            || !self.settings.has_locations()
        {
            return;
        }
        match self.settings.auto_rebuild {
            AutoRebuild::Never => {}
            AutoRebuild::OnStart => {
                if !self.auto_started {
                    self.auto_started = true;
                    self.rebuild_index();
                }
            }
            every => {
                let Some(interval) = every.interval_secs() else {
                    return;
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let due = self.settings.last_rebuild.map_or(now, |t| t + interval);
                if now >= due {
                    self.rebuild_index();
                } else {
                    ctx.request_repaint_after(Duration::from_secs(due - now));
                }
            }
        }
    }

    fn sync_theme(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let dark = ctx.theme() == egui::Theme::Dark;
        if dark != self.dark {
            self.dark = dark;
            self.palette = Palette::new(dark, self.accent);
            theme::apply_style(ctx, &self.palette);
            platform::set_frame_theme(frame, dark);
        }
    }

    fn row_count(&self) -> usize {
        match (&self.hits, &self.index) {
            (Some(h), Some(i)) => h.len(i),
            _ => 0,
        }
    }

    fn path_of_row(&self, row: usize) -> Option<PathBuf> {
        let index = self.index.as_ref()?;
        let id = self.hits.as_ref()?.get(index, row)?;
        Some(index.path(id))
    }

    fn open_selected(&self, reveal: bool) {
        if let Some(path) = self.selected.and_then(|r| self.path_of_row(r)) {
            if reveal {
                platform::reveal(&path);
            } else {
                platform::open(&path);
            }
        }
    }

    fn copy_selected(&self, name_only: bool) {
        let Some(path) = self.selected.and_then(|r| self.path_of_row(r)) else {
            return;
        };
        let text = if name_only {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        } else {
            path.display().to_string()
        };
        platform::copy_text(&text);
    }

    fn handle_keys(&mut self, ctx: &egui::Context, visible_rows: usize) {
        let rows = self.row_count();
        let mut moved = None::<i64>;
        ctx.input_mut(|i| {
            let page = visible_rows.max(1) as i64;
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                moved = Some(1);
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                moved = Some(-1);
            }
            if i.consume_key(Modifiers::NONE, Key::PageDown) {
                moved = Some(page);
            }
            if i.consume_key(Modifiers::NONE, Key::PageUp) {
                moved = Some(-page);
            }
            if i.consume_key(Modifiers::CTRL, Key::Home) {
                moved = Some(i64::MIN);
            }
            if i.consume_key(Modifiers::CTRL, Key::End) {
                moved = Some(i64::MAX);
            }
        });
        if let Some(delta) = moved
            && rows > 0
        {
            let current = self.selected.map(|s| s as i64).unwrap_or(-1);
            let target = match delta {
                i64::MIN => 0,
                i64::MAX => rows as i64 - 1,
                d => (current.saturating_add(d)).clamp(0, rows as i64 - 1),
            };
            self.selected = Some(target as usize);
            self.follow_selection = true;
        }
        let (enter, ctrl_enter, copy_path, escape) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::CTRL, Key::Enter),
                i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::C),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if enter {
            self.open_selected(false);
        }
        if ctrl_enter {
            self.open_selected(true);
        }
        if copy_path {
            self.copy_selected(false);
        }
        if escape {
            self.query.clear();
            self.focus_search = true;
        }
    }

    // ---------------------------------------------------------------
    // Drawing

    fn search_box(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let p = self.palette;
        let id = egui::Id::unique("search");
        let focused = ui.memory(|m| m.has_focus(id));
        let hovered = ui.rect_contains_pointer(rect);
        let fill = if focused {
            if p.dark {
                Color32::from_rgba_unmultiplied(0x1E, 0x1E, 0x1E, 0xB3)
            } else {
                Color32::WHITE
            }
        } else if hovered {
            p.control_hover
        } else {
            p.control
        };
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(6),
            fill,
            Stroke::new(1.0, p.control_stroke),
            StrokeKind::Inside,
        );
        // Fluent focus: a 2 px accent underline inside the rounded box.
        if focused {
            let underline = Rect::from_min_max(
                pos2(rect.left() + 1.0, rect.bottom() - 2.0),
                pos2(rect.right() - 1.0, rect.bottom()),
            );
            painter.rect_filled(
                underline,
                CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 6,
                    se: 6,
                },
                p.accent_text,
            );
        }
        icon_search(
            painter,
            pos2(rect.left() + 20.0, rect.center().y),
            p.text_secondary,
        );

        let clear_w = if self.query.is_empty() { 0.0 } else { 32.0 };
        let text_rect = Rect::from_min_max(
            pos2(rect.left() + 40.0, rect.top()),
            pos2(rect.right() - 12.0 - clear_w, rect.bottom()),
        );
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(text_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        // Paste from the real system clipboard (the egui clipboard
        // feature is off; see Cargo.toml).
        if focused {
            let pasted = ui.input_mut(|i| {
                let had = i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)))
                    || i.consume_key(Modifiers::CTRL, Key::V);
                i.events.retain(|e| !matches!(e, egui::Event::Paste(_)));
                had
            });
            if pasted && let Some(text) = platform::paste_text() {
                let line = text.lines().next().unwrap_or("").to_owned();
                insert_at_cursor(ui.ctx(), id, &mut self.query, &line);
            }
        }
        let edit = egui::TextEdit::singleline(&mut self.query)
            .id(id)
            .frame(egui::Frame::NONE)
            .hint_text(egui::RichText::new("Search files and folders").color(p.text_tertiary))
            .font(FontId::proportional(15.0))
            .text_color(p.text)
            .desired_width(text_rect.width())
            .align(Align2::LEFT_CENTER);
        let response = child.add(edit);
        if self.focus_search {
            response.request_focus();
            self.focus_search = false;
        }
        if clear_w > 0.0 {
            let c = pos2(rect.right() - 24.0, rect.center().y);
            let r = Rect::from_center_size(c, vec2(28.0, 26.0));
            let resp = ui.interact(r, egui::Id::unique("clear"), Sense::click());
            if resp.hovered() {
                ui.painter()
                    .rect_filled(r, CornerRadius::same(4), p.row_hover);
            }
            icon_close(ui.painter(), c, p.text_secondary);
            if resp.on_hover_cursor(CursorIcon::Default).clicked() {
                self.query.clear();
                self.focus_search = true;
            }
        }
    }

    fn results(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        let outer = ui.available_rect_before_wrap();
        ui.allocate_rect(outer, Sense::hover());
        let painter = ui.painter_at(outer);
        painter.rect(
            outer,
            CornerRadius::same(8),
            p.card,
            Stroke::new(1.0, p.card_stroke),
            StrokeKind::Inside,
        );

        match &self.state {
            State::Ready => {}
            State::Loading => return self.placeholder(ui, outer, "Loading index…", None),
            State::Scanning(msg) => {
                let msg = msg.clone();
                return self.placeholder(
                    ui,
                    outer,
                    &msg,
                    Some("Search becomes available when the scan finishes."),
                );
            }
            State::Failed(msg) => {
                let msg = msg.clone();
                return self.placeholder(ui, outer, "Something went wrong", Some(&msg));
            }
            State::NoIndex { path } => {
                let detail = match path {
                    Some(p) => format!("No index yet at {}.", p.display()),
                    None => "No index location is available.".to_owned(),
                };
                return self.empty_state(ui, outer, &detail);
            }
        }
        let Some(index) = self.index.clone() else {
            return;
        };

        if let Some(err) = &self.query_error {
            let err = err.clone();
            return self.placeholder(ui, outer, "Check the search text", Some(&err));
        }
        let Some(hits) = self.hits.clone() else {
            return;
        };
        let rows = hits.len(&index);
        if rows == 0 {
            return self.placeholder(ui, outer, "No results", Some("Try fewer or shorter words."));
        }

        // Header.
        let inner = outer.shrink(1.0);
        let header = Rect::from_min_size(inner.min, vec2(inner.width(), HEADER_HEIGHT));
        let list = Rect::from_min_max(pos2(inner.left(), header.bottom()), inner.max);
        let content_w = list.width() - SCROLLBAR_WIDTH;
        if self.name_width <= 0.0 {
            self.name_width = (content_w * 0.36).clamp(180.0, 420.0);
        }
        let type_w = 120.0_f32.min(content_w * 0.2);
        let name_w = self
            .name_width
            .clamp(140.0, (content_w - type_w - 120.0).max(140.0));
        let col_name = inner.left() + 12.0;
        let col_path = col_name + name_w;
        let col_type = inner.left() + content_w - type_w;

        let header_font = FontId::new(12.0, FontFamily::Name(theme::SEMIBOLD.into()));
        painter.text(
            pos2(col_name + 28.0, header.center().y),
            Align2::LEFT_CENTER,
            "Name",
            header_font.clone(),
            p.text_secondary,
        );
        icon_sort_up(
            &painter,
            pos2(col_name + 28.0 + 44.0, header.center().y),
            p.text_tertiary,
        );
        painter.text(
            pos2(col_path + 8.0, header.center().y),
            Align2::LEFT_CENTER,
            "Folder",
            header_font.clone(),
            p.text_secondary,
        );
        painter.text(
            pos2(col_type + 8.0, header.center().y),
            Align2::LEFT_CENTER,
            "Type",
            header_font,
            p.text_secondary,
        );
        // Column dividers in the header, resizable name column.
        for x in [col_path, col_type] {
            painter.line_segment(
                [pos2(x, header.top() + 8.0), pos2(x, header.bottom() - 8.0)],
                Stroke::new(1.0, p.divider),
            );
        }
        let grip =
            Rect::from_center_size(pos2(col_path, header.center().y), vec2(8.0, HEADER_HEIGHT));
        let grip_resp = ui
            .interact(grip, egui::Id::unique("name-col"), Sense::drag())
            .on_hover_cursor(CursorIcon::ResizeColumn);
        if grip_resp.dragged() {
            self.name_width += grip_resp.drag_delta().x;
        }
        painter.line_segment(
            [
                pos2(inner.left(), header.bottom()),
                pos2(inner.right(), header.bottom()),
            ],
            Stroke::new(1.0, p.divider),
        );

        // Scrolling, in rows (f64 so millions of rows stay precise).
        let visible = (list.height() / ROW_HEIGHT).max(1.0);
        let max_scroll = (rows as f64 - visible as f64).max(0.0);
        let list_resp = ui.interact(list, egui::Id::unique("list"), Sense::click());
        if ui.rect_contains_pointer(list) {
            // Scroll animation is off (theme.rs), so this is the raw wheel
            // delta: one repaint per notch, no animated frames.
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                self.scroll -= (dy / ROW_HEIGHT) as f64;
            }
        }
        if self.follow_selection
            && let Some(sel) = self.selected
        {
            let sel = sel as f64;
            let fully = (visible.floor() as f64).max(1.0);
            if sel < self.scroll {
                self.scroll = sel;
            } else if sel + 1.0 > self.scroll + fully {
                self.scroll = sel + 1.0 - fully;
            }
            self.follow_selection = false;
        }

        // Scrollbar.
        let track = Rect::from_min_max(
            pos2(list.right() - SCROLLBAR_WIDTH, list.top() + 4.0),
            pos2(list.right() - 2.0, list.bottom() - 4.0),
        );
        if max_scroll > 0.0 {
            let ratio = (visible as f64 / rows as f64).clamp(0.0, 1.0) as f32;
            let thumb_h = (track.height() * ratio).max(24.0);
            let travel = track.height() - thumb_h;
            let bar_resp = ui.interact(
                track,
                egui::Id::unique("scrollbar"),
                Sense::click_and_drag(),
            );
            if bar_resp.dragged() {
                self.scroll += (bar_resp.drag_delta().y / travel.max(1.0)) as f64 * max_scroll;
            } else if bar_resp.clicked()
                && let Some(pos) = bar_resp.interact_pointer_pos()
            {
                let t = ((pos.y - track.top() - thumb_h / 2.0) / travel.max(1.0)).clamp(0.0, 1.0);
                self.scroll = t as f64 * max_scroll;
            }
            self.scroll = self.scroll.clamp(0.0, max_scroll);
            let t = (self.scroll / max_scroll) as f32;
            let active = bar_resp.hovered() || bar_resp.dragged();
            let width = if active { 6.0 } else { 3.0 };
            let thumb = Rect::from_min_size(
                pos2(track.right() - width - 1.0, track.top() + travel * t),
                vec2(width, thumb_h),
            );
            painter.rect_filled(
                thumb,
                CornerRadius::same(3),
                if active {
                    p.text_secondary
                } else {
                    p.text_tertiary
                },
            );
        } else {
            self.scroll = 0.0;
        }

        // Rows.
        let first = self.scroll.floor() as usize;
        let offset = ((self.scroll - first as f64) as f32) * ROW_HEIGHT;
        let count = (visible.ceil() as usize + 1).min(rows.saturating_sub(first));
        let row_painter = ui.painter_at(list);
        let hover_pos = ui
            .input(|i| i.pointer.hover_pos())
            .filter(|p| list.contains(*p) && p.x < track.left());
        let terms = highlight_terms(&self.query);
        let name_font = FontId::proportional(14.0);
        let sub_font = FontId::proportional(13.0);
        let mut clicked_row = None;
        for i in 0..count {
            let row = first + i;
            let Some(id) = hits.get(&index, row) else {
                break;
            };
            let top = list.top() + i as f32 * ROW_HEIGHT - offset;
            let rect = Rect::from_min_size(
                pos2(list.left() + 4.0, top + 2.0),
                vec2(content_w - 6.0, ROW_HEIGHT - 4.0),
            );
            let is_sel = self.selected == Some(row);
            let is_hover = hover_pos.is_some_and(|h| h.y >= top && h.y < top + ROW_HEIGHT);
            if is_sel {
                row_painter.rect_filled(rect, CornerRadius::same(4), p.row_selected);
                // WinUI list selection pill.
                let pill = Rect::from_center_size(
                    pos2(rect.left() + 1.5, rect.center().y),
                    vec2(3.0, 16.0),
                );
                row_painter.rect_filled(pill, CornerRadius::same(2), p.accent_text);
            } else if is_hover && !self.remote {
                // No hover fills over RDP: each hover change is a repaint.
                row_painter.rect_filled(rect, CornerRadius::same(4), p.row_hover);
            }
            if is_hover {
                clicked_row = Some(row);
            }
            let cy = rect.center().y;
            let is_dir = index.flags(id) & flags::DIR != 0;
            if is_dir {
                icon_folder(&row_painter, pos2(col_name + 8.0, cy), &p);
            } else {
                icon_file(&row_painter, pos2(col_name + 8.0, cy), &p);
            }
            let name = String::from_utf8_lossy(index.name(id));
            let name_clip = Rect::from_min_max(
                pos2(col_name + 28.0, rect.top()),
                pos2(col_path - 8.0, rect.bottom()),
            );
            let mut job = highlighted(&name, &terms, name_font.clone(), p.text, p.accent_text);
            job.wrap.max_width = name_clip.width();
            job.wrap.overflow_character = Some('…');
            job.wrap.break_anywhere = true;
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            row_painter.with_clip_rect(name_clip).galley(
                pos2(name_clip.left(), cy - galley.size().y / 2.0),
                galley,
                p.text,
            );

            let parent = index.parent(id);
            let folder = if parent == ferrow_index::ROOT_PARENT {
                String::new()
            } else {
                index.path(parent).display().to_string()
            };
            let folder_clip = Rect::from_min_max(
                pos2(col_path + 8.0, rect.top()),
                pos2(col_type - 8.0, rect.bottom()),
            );
            let galley = ellipsized(
                ui,
                folder,
                sub_font.clone(),
                p.text_secondary,
                folder_clip.width(),
            );
            row_painter.with_clip_rect(folder_clip).galley(
                pos2(folder_clip.left(), cy - galley.size().y / 2.0),
                galley,
                p.text_secondary,
            );

            let kind = type_label(&name, is_dir);
            let type_clip = Rect::from_min_max(
                pos2(col_type + 8.0, rect.top()),
                pos2(rect.right(), rect.bottom()),
            );
            row_painter.with_clip_rect(type_clip).text(
                pos2(type_clip.left(), cy),
                Align2::LEFT_CENTER,
                kind,
                sub_font.clone(),
                p.text_secondary,
            );
        }

        // Mouse: click selects, double-click opens, right-click menu.
        if (list_resp.clicked() || list_resp.secondary_clicked() || list_resp.double_clicked())
            && let Some(row) = clicked_row
        {
            self.selected = Some(row);
        }
        if list_resp.double_clicked() && clicked_row.is_some() {
            self.open_selected(false);
        }
        let has_sel = self.selected.is_some();
        list_resp.context_menu(|ui| {
            ui.set_min_width(220.0);
            if menu_item(ui, "Open", "Enter", has_sel) {
                self.open_selected(false);
                ui.close();
            }
            if menu_item(ui, "Open file location", "Ctrl+Enter", has_sel) {
                self.open_selected(true);
                ui.close();
            }
            ui.separator();
            if menu_item(ui, "Copy full path", "Ctrl+Shift+C", has_sel) {
                self.copy_selected(false);
                ui.close();
            }
            if menu_item(ui, "Copy name", "", has_sel) {
                self.copy_selected(true);
                ui.close();
            }
        });
    }

    fn placeholder(&self, ui: &mut egui::Ui, rect: Rect, title: &str, detail: Option<&str>) {
        let p = self.palette;
        let painter = ui.painter_at(rect);
        let c = rect.center();
        painter.text(
            c - vec2(0.0, 12.0),
            Align2::CENTER_CENTER,
            title,
            FontId::new(18.0, FontFamily::Name(theme::SEMIBOLD.into())),
            p.text,
        );
        if let Some(d) = detail {
            painter.text(
                c + vec2(0.0, 16.0),
                Align2::CENTER_CENTER,
                d,
                FontId::proportional(13.0),
                p.text_secondary,
            );
        }
    }

    fn empty_state(&mut self, ui: &mut egui::Ui, rect: Rect, detail: &str) {
        let p = self.palette;
        let c = rect.center();
        let painter = ui.painter_at(rect);
        painter.text(
            c - vec2(0.0, 44.0),
            Align2::CENTER_CENTER,
            "Build your first index",
            FontId::new(20.0, FontFamily::Name(theme::SEMIBOLD.into())),
            p.text,
        );
        painter.text(
            c - vec2(0.0, 14.0),
            Align2::CENTER_CENTER,
            detail,
            FontId::proportional(13.0),
            p.text_secondary,
        );
        let home = home_dir();
        let label = "Index my home folder";
        let btn = Rect::from_center_size(c + vec2(0.0, 26.0), vec2(200.0, 32.0));
        let resp = ui.interact(btn, egui::Id::unique("index-home"), Sense::click());
        let fill = if resp.is_pointer_button_down_on() {
            p.accent.gamma_multiply(0.85)
        } else if resp.hovered() {
            p.accent.gamma_multiply(0.92)
        } else {
            p.accent
        };
        painter.rect_filled(btn, CornerRadius::same(4), fill);
        painter.text(
            btn.center(),
            Align2::CENTER_CENTER,
            label,
            FontId::new(14.0, FontFamily::Name(theme::SEMIBOLD.into())),
            Color32::WHITE,
        );
        if resp.on_hover_cursor(CursorIcon::Default).clicked()
            && let Some(home) = home
        {
            self.index_home(home);
        }
        painter.text(
            c + vec2(0.0, 64.0),
            Align2::CENTER_CENTER,
            "Other folders: ferrow index <folder>",
            FontId::proportional(12.0),
            p.text_tertiary,
        );
    }

    /// Settings button (gear) in the title row.
    fn gear_button(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let p = self.palette;
        let resp = ui.interact(rect, egui::Id::unique("gear"), Sense::click());
        let on = self.page == Page::Settings;
        let inner = rect.shrink2(vec2(4.0, 6.0));
        if resp.hovered() || on {
            ui.painter().rect_filled(
                inner,
                CornerRadius::same(4),
                if on { p.row_selected } else { p.row_hover },
            );
        }
        let c = rect.center();
        if theme::has_icon_font(ui.ctx()) {
            ui.painter().text(
                c,
                Align2::CENTER_CENTER,
                "\u{E713}",
                FontId::new(14.0, FontFamily::Name(theme::ICONS.into())),
                p.text,
            );
        } else {
            ui.painter().circle_stroke(c, 5.0, Stroke::new(1.5, p.text));
        }
        if resp.on_hover_text("Settings").clicked() {
            self.page = if on { Page::Results } else { Page::Settings };
            self.focus_search = on;
        }
    }

    /// Minimise, maximise/restore and close, drawn like Windows 11 with
    /// the system Segoe Fluent Icons glyphs.
    fn caption_buttons(&self, ui: &mut egui::Ui, rect: Rect) {
        let p = self.palette;
        let ctx = ui.ctx().clone();
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let active = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let glyph_color = if active { p.text } else { p.text_tertiary };
        let icons = FontId::new(10.0, FontFamily::Name(theme::ICONS.into()));
        let has_icons = theme::has_icon_font(&ctx);
        #[cfg(windows)]
        let (max_hover, max_pressed) = crate::frame::max_button_state();
        #[cfg(not(windows))]
        let (max_hover, max_pressed) = (false, false);
        let painter = ui.painter_at(rect);
        let w = CAPTION_BUTTON_WIDTH;
        for (i, kind) in ["min", "max", "close"].into_iter().enumerate() {
            let r = Rect::from_min_size(
                pos2(rect.left() + w * i as f32, rect.top()),
                vec2(w, rect.height()),
            );
            let (hovered, pressed, clicked) = if kind == "max" {
                (max_hover, max_pressed, false)
            } else {
                let resp = ui.interact(r, egui::Id::unique(("caption", kind)), Sense::click());
                (
                    resp.hovered(),
                    resp.is_pointer_button_down_on(),
                    resp.clicked(),
                )
            };
            let close_red = Color32::from_rgb(0xC4, 0x2B, 0x1C);
            let (fill, color) = match (kind, hovered, pressed) {
                ("close", true, true) => (close_red.gamma_multiply(0.9), Color32::WHITE),
                ("close", true, false) => (close_red, Color32::WHITE),
                (_, true, true) => (p.row_selected.gamma_multiply(0.7), glyph_color),
                (_, true, false) => (p.row_hover, glyph_color),
                _ => (Color32::TRANSPARENT, glyph_color),
            };
            painter.rect_filled(r, CornerRadius::ZERO, fill);
            let c = r.center();
            if has_icons {
                let glyph = match kind {
                    "min" => "\u{E921}",
                    "max" if maximized => "\u{E923}",
                    "max" => "\u{E922}",
                    _ => "\u{E8BB}",
                };
                painter.text(c, Align2::CENTER_CENTER, glyph, icons.clone(), color);
            } else {
                let s = Stroke::new(1.0, color);
                match kind {
                    "min" => {
                        painter.line_segment([c + vec2(-5.0, 0.0), c + vec2(5.0, 0.0)], s);
                    }
                    "max" => {
                        painter.rect_stroke(
                            Rect::from_center_size(c, vec2(10.0, 10.0)),
                            CornerRadius::same(1),
                            s,
                            StrokeKind::Inside,
                        );
                    }
                    _ => icon_close(&painter, c, color),
                }
            }
            if clicked {
                match kind {
                    "min" => ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                    "close" => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                    _ => {}
                }
            }
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        let p = self.palette;
        let rect = ui.max_rect();
        let painter = ui.painter();
        let font = FontId::proportional(12.0);
        let left = match (&self.state, &self.index) {
            (State::Ready, Some(index)) => {
                let rows = self.row_count();
                let results = if self.query.trim().is_empty() {
                    format!("{} objects", group(index.len()))
                } else if self.complete {
                    format!(
                        "{} {}",
                        group(rows),
                        if rows == 1 { "result" } else { "results" }
                    )
                } else {
                    format!("{}+ results", group(rows))
                };
                format!("{results}   ·   {}", fmt_ms(self.search_time))
            }
            _ => String::new(),
        };
        painter.text(
            pos2(rect.left() + 4.0, rect.center().y),
            Align2::LEFT_CENTER,
            left,
            font.clone(),
            p.text_secondary,
        );
        if let (State::Ready, Some(index)) = (&self.state, &self.index) {
            let right = format!(
                "Local index  ·  {} entries  ·  loaded in {}",
                group(index.len()),
                fmt_ms(self.load_time)
            );
            let resp = ui.interact(rect, egui::Id::unique("status"), Sense::hover());
            painter.text(
                pos2(rect.right() - 4.0, rect.center().y),
                Align2::RIGHT_CENTER,
                right,
                font,
                p.text_tertiary,
            );
            resp.on_hover_text(&self.index_source);
        }
    }
}

impl eframe::App for FerrowApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.backdrop {
            [0.0; 4]
        } else {
            self.palette.window.to_normalized_gamma_f32()
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.sync_theme(&ctx, frame);
        self.drain_events();
        self.auto_rebuild(&ctx);

        // Every keystroke starts a new (cancelling) search on the worker.
        if self.page == Page::Settings
            && self.sent_query.is_some()
            && self.sent_query.as_deref() != Some(self.query.as_str())
        {
            self.page = Page::Results;
        }
        if matches!(self.state, State::Ready)
            && self.sent_query.as_deref() != Some(self.query.as_str())
        {
            self.engine.search(&self.query);
            self.sent_query = Some(self.query.clone());
        }

        let full = ui.max_rect();
        let ppp = ctx.pixels_per_point();
        let title_h = if platform::CUSTOM_TITLE_ROW {
            TITLE_HEIGHT
        } else {
            0.0
        };
        let p = self.palette;
        // Title row: app mark, name and the search box; on Windows it is
        // the window's title bar (the system caption buttons sit on top
        // at the right).
        let search_rect = if platform::CUSTOM_TITLE_ROW {
            let painter = ui.painter();
            let mid = full.top() + title_h / 2.0;
            if let Some(tex) = self.title_logo.texture(&ctx, 24.0) {
                let r = Rect::from_center_size(pos2(full.left() + 24.0, mid), vec2(24.0, 24.0));
                painter.image(
                    tex.id(),
                    r,
                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            painter.text(
                pos2(full.left() + 44.0, mid),
                Align2::LEFT_CENTER,
                "Ferrow",
                FontId::proportional(13.0),
                p.text_secondary,
            );
            let left = full.left() + 110.0;
            let right = full.right() - CAPTION_BUTTONS_WIDTH - GEAR_WIDTH - 8.0;
            let width = (right - left).clamp(120.0, 640.0);
            let x = (full.center().x - width / 2.0).max(left).min(right - width);
            let r = Rect::from_min_size(pos2(x, mid - 15.0), vec2(width, 30.0));
            let buttons_left = full.right() - CAPTION_BUTTONS_WIDTH;
            let gear = Rect::from_min_max(
                pos2(buttons_left - GEAR_WIDTH, full.top()),
                pos2(buttons_left, full.top() + title_h),
            );
            self.gear_button(ui, gear);
            let max_button = (
                buttons_left + CAPTION_BUTTON_WIDTH,
                buttons_left + 2.0 * CAPTION_BUTTON_WIDTH,
            );
            #[cfg(windows)]
            crate::frame::set_caption(&crate::frame::Caption {
                height: title_h * ppp,
                control: (r.left() * ppp, r.right() * ppp),
                buttons_left: gear.left() * ppp,
                max_button: (max_button.0 * ppp, max_button.1 * ppp),
            });
            let _ = max_button;
            self.caption_buttons(
                ui,
                Rect::from_min_max(
                    pos2(buttons_left, full.top()),
                    pos2(full.right(), full.top() + title_h),
                ),
            );
            r
        } else {
            Rect::from_min_size(full.min + vec2(12.0, 10.0), vec2(full.width() - 24.0, 36.0))
        };
        self.search_box(ui, search_rect);
        let top = if platform::CUSTOM_TITLE_ROW {
            full.top() + title_h
        } else {
            search_rect.bottom() + 10.0
        };
        let content = Rect::from_min_max(
            pos2(full.left() + 8.0, top),
            pos2(full.right() - 8.0, full.bottom() - 2.0),
        );
        if self.page == Page::Settings {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.page = Page::Results;
                self.focus_search = true;
            }
            let mut page_ui = ui.new_child(egui::UiBuilder::new().max_rect(content));
            self.settings_page(&mut page_ui);
        } else {
            let list_h = (content.height() - STATUS_HEIGHT).max(0.0);
            let visible_rows = ((list_h - HEADER_HEIGHT) / ROW_HEIGHT).floor().max(1.0) as usize;
            self.handle_keys(&ctx, visible_rows);
            let list_rect = Rect::from_min_size(content.min, vec2(content.width(), list_h));
            let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(list_rect));
            self.results(&mut list_ui);
            let status_rect = Rect::from_min_max(
                pos2(content.left() + 6.0, list_rect.bottom()),
                pos2(content.right() - 6.0, content.bottom()),
            );
            let mut status_ui = ui.new_child(egui::UiBuilder::new().max_rect(status_rect));
            self.status_bar(&mut status_ui);
        }

        // Forward egui copy commands (Ctrl+C in the search box) to the
        // system clipboard.
        let copied: Vec<String> = ctx.output(|o| {
            o.commands
                .iter()
                .filter_map(|c| match c {
                    egui::OutputCommand::CopyText(t) => Some(t.clone()),
                    _ => None,
                })
                .collect()
        });
        if let Some(text) = copied.last() {
            platform::copy_text(text);
        }
    }
}

// -------------------------------------------------------------------
// Helpers

fn insert_at_cursor(ctx: &egui::Context, id: egui::Id, text: &mut String, insert: &str) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    let chars = text.chars().count();
    let (start, end) = state
        .cursor
        .char_range()
        .map(|r| {
            let (a, b) = (
                r.primary.index.0.min(r.secondary.index.0),
                r.primary.index.0.max(r.secondary.index.0),
            );
            (a.min(chars), b.min(chars))
        })
        .unwrap_or((chars, chars));
    let byte = |c: usize| {
        text.char_indices()
            .nth(c)
            .map(|(b, _)| b)
            .unwrap_or(text.len())
    };
    let (bs, be) = (byte(start), byte(end));
    text.replace_range(bs..be, insert);
    let cursor = egui::text::CCursor::new(start + insert.chars().count());
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(cursor)));
    state.store(ctx, id);
}

fn ellipsized(
    ui: &egui::Ui,
    text: String,
    font: FontId,
    color: Color32,
    width: f32,
) -> Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text, font, color);
    job.wrap.max_width = width;
    job.wrap.max_rows = 1;
    job.wrap.overflow_character = Some('…');
    job.wrap.break_anywhere = true;
    ui.fonts_mut(|f| f.layout_job(job))
}

fn menu_item(ui: &mut egui::Ui, label: &str, shortcut: &str, enabled: bool) -> bool {
    let button = egui::Button::new(label)
        .shortcut_text(shortcut)
        .min_size(vec2(200.0, 30.0));
    ui.add_enabled(enabled, button).clicked()
}

/// Lower-cased, non-negated search words, for match highlighting.
fn highlight_terms(query: &str) -> Vec<String> {
    let Ok(q) = ferrow_query::Query::parse(query) else {
        return Vec::new();
    };
    q.terms
        .into_iter()
        .filter(|t| !t.negated && !t.text.is_empty())
        .map(|t| {
            if t.case_sensitive {
                t.text
            } else {
                t.text.to_lowercase()
            }
        })
        .collect()
}

/// Lay out `name` with every occurrence of the search words highlighted.
fn highlighted(
    name: &str,
    terms: &[String],
    font: FontId,
    color: Color32,
    highlight: Color32,
) -> LayoutJob {
    let lower = name.to_lowercase();
    // Byte ranges to mark; lower-casing can change byte lengths, so only
    // highlight when the lengths agree (always true for ASCII).
    let mut marks = vec![false; name.len()];
    if lower.len() == name.len() {
        for term in terms {
            let mut from = 0;
            while let Some(pos) = lower[from..].find(term.as_str()) {
                let s = from + pos;
                marks[s..s + term.len()].iter_mut().for_each(|m| *m = true);
                from = s + term.len().max(1);
            }
        }
    }
    let mut job = LayoutJob::default();
    let mut start = 0;
    while start < name.len() {
        let on = marks[start];
        let mut end = start + 1;
        while end < name.len() && (marks[end] == on || !name.is_char_boundary(end)) {
            end += 1;
        }
        let fmt = egui::TextFormat::simple(font.clone(), if on { highlight } else { color });
        job.append(&name[start..end], 0.0, fmt);
        start = end;
    }
    job.wrap.max_rows = 1;
    job
}

fn type_label(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "Folder".into();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() <= 12 => {
            format!("{} file", ext.to_uppercase())
        }
        _ => "File".into(),
    }
}

fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn fmt_ms(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 10.0 {
        format!("{ms:.1} ms")
    } else if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

// Icons are drawn as vector shapes, so no icon font or image asset is
// needed and they stay crisp at every display scale.

fn icon_search(painter: &egui::Painter, c: Pos2, color: Color32) {
    let center = c + vec2(-1.5, -1.5);
    painter.circle_stroke(center, 5.5, Stroke::new(1.4, color));
    painter.line_segment(
        [center + vec2(4.0, 4.0), center + vec2(8.0, 8.0)],
        Stroke::new(1.6, color),
    );
}

fn icon_close(painter: &egui::Painter, c: Pos2, color: Color32) {
    let s = 4.5;
    let stroke = Stroke::new(1.3, color);
    painter.line_segment([c + vec2(-s, -s), c + vec2(s, s)], stroke);
    painter.line_segment([c + vec2(-s, s), c + vec2(s, -s)], stroke);
}

fn icon_sort_up(painter: &egui::Painter, c: Pos2, color: Color32) {
    let stroke = Stroke::new(1.2, color);
    painter.line_segment([c + vec2(-3.5, 1.5), c + vec2(0.0, -2.0)], stroke);
    painter.line_segment([c + vec2(0.0, -2.0), c + vec2(3.5, 1.5)], stroke);
}

fn icon_folder(painter: &egui::Painter, c: Pos2, p: &Palette) {
    let body = Rect::from_center_size(c + vec2(0.0, 1.0), vec2(16.0, 11.0));
    let tab = Rect::from_min_size(pos2(body.left(), body.top() - 2.5), vec2(7.0, 4.0));
    painter.rect_filled(tab, CornerRadius::same(1), p.folder_dark);
    painter.rect_filled(body, CornerRadius::same(2), p.folder);
}

fn icon_file(painter: &egui::Painter, c: Pos2, p: &Palette) {
    let r = Rect::from_center_size(c, vec2(12.0, 15.0));
    let fold = 4.0;
    let fill = if p.dark {
        Color32::from_rgb(0xDA, 0xDA, 0xDA)
    } else {
        Color32::WHITE
    };
    let edge = if p.dark {
        Color32::from_rgb(0x9A, 0x9A, 0x9A)
    } else {
        Color32::from_rgb(0x8A, 0x8A, 0x8A)
    };
    let pts = vec![
        r.left_top(),
        pos2(r.right() - fold, r.top()),
        pos2(r.right(), r.top() + fold),
        r.right_bottom(),
        r.left_bottom(),
    ];
    painter.add(Shape::convex_polygon(
        pts.clone(),
        fill,
        Stroke::new(1.0, edge),
    ));
    painter.line_segment(
        [
            pos2(r.right() - fold, r.top()),
            pos2(r.right() - fold, r.top() + fold),
        ],
        Stroke::new(1.0, edge),
    );
    painter.line_segment(
        [
            pos2(r.right() - fold, r.top() + fold),
            pos2(r.right(), r.top() + fold),
        ],
        Stroke::new(1.0, edge),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(5_000_000), "5,000,000");
    }

    #[test]
    fn type_labels() {
        assert_eq!(type_label("a.rs", false), "RS file");
        assert_eq!(type_label(".gitignore", false), "File");
        assert_eq!(type_label("src", true), "Folder");
    }

    #[test]
    fn highlight_marks_case_insensitive_matches() {
        let job = highlighted(
            "ReadMe.md",
            &["readme".into()],
            FontId::proportional(14.0),
            Color32::WHITE,
            Color32::RED,
        );
        assert_eq!(job.sections.len(), 2);
        assert_eq!(job.sections[0].format.color, Color32::RED);
        assert_eq!(
            &job.text[job.sections[1].byte_range.start.0..job.sections[1].byte_range.end.0],
            ".md"
        );
    }

    #[test]
    fn highlight_handles_non_ascii() {
        let job = highlighted(
            "Ärger.txt",
            &["ä".into()],
            FontId::proportional(14.0),
            Color32::WHITE,
            Color32::RED,
        );
        assert_eq!(job.text, "Ärger.txt");
    }
}
