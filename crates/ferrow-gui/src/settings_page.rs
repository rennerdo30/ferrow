//! The settings page, laid out like Windows 11 Settings: a page title,
//! a section list on the left (wide windows) and cards with one setting per
//! row. The most important fact comes first: whether the fast, live index of
//! the Ferrow service is on, and if not, why.

use std::path::PathBuf;

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontFamily, FontId, Layout, RichText, Sense, Stroke,
    StrokeKind, Vec2, vec2,
};

use super::{FerrowApp, Page, PickTarget};
use crate::access;
use crate::platform;
use crate::settings::{SourceKind, ThemeChoice, UpdateChannel};
use crate::theme;
use crate::update::{self, UpdateState};

const CARD_GAP: f32 = 4.0;
const SECTION_GAP: f32 = 20.0;
const MAX_WIDTH: f32 = 860.0;
/// From this window width on, a section list stands left of the cards.
const NAV_MIN_WIDTH: f32 = 900.0;
const NAV_WIDTH: f32 = 184.0;
const NAV_GAP: f32 = 12.0;
const TITLE_HEIGHT: f32 = 52.0;
/// Space between a row's text and its control.
const CONTROL_GAP: f32 = 16.0;
const MIN_TEXT_WIDTH: f32 = 140.0;

/// The page's sections, in order: what the list on the left shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Section {
    FastIndex,
    OwnIndex,
    Windows,
    Appearance,
    Updates,
    About,
}

impl Section {
    const ALL: [Section; 6] = [
        Section::FastIndex,
        Section::OwnIndex,
        Section::Windows,
        Section::Appearance,
        Section::Updates,
        Section::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::FastIndex => "Fast index",
            Section::OwnIndex => "Own index",
            Section::Windows => "Windows",
            Section::Appearance => "Appearance",
            Section::Updates => "Updates",
            Section::About => "About",
        }
    }

    fn shown(self) -> bool {
        self != Section::Windows || cfg!(windows)
    }

    /// The section a "scroll to" request names (the list's labels, and
    /// "Service access", which lives in the fast index section).
    pub(super) fn from_target(name: &str) -> Option<Section> {
        let name = if name == "Service access" {
            "Fast index"
        } else {
            name
        };
        Section::ALL
            .into_iter()
            .find(|s| s.shown() && s.label().eq_ignore_ascii_case(name))
    }
}

/// How far down the view the highlight line sits, as a share of its height:
/// a section owns the view once its heading has scrolled above this line.
const HIGHLIGHT_LINE: f32 = 0.4;

/// The line a heading must be above to take the highlight in a scroll area
/// that starts at `viewport_top` and is `viewport_h` tall.
fn highlight_line(viewport_top: f32, viewport_h: f32) -> f32 {
    viewport_top + viewport_h * HIGHLIGHT_LINE
}

/// The last section whose top is at or above `line`, or the first one before
/// any is reached; the last section when the page is scrolled to its end.
/// `tops` are in page order.
fn section_in_view(tops: &[(Section, f32)], line: f32, at_end: bool) -> Section {
    if at_end && let Some((last, _)) = tops.last() {
        return *last;
    }
    tops.iter()
        .take_while(|(_, top)| *top <= line)
        .last()
        .or(tops.first())
        .map_or(Section::FastIndex, |(s, _)| *s)
}

/// A heading this close to the top of the scroll area (either side) owns the
/// view, whatever follows it: a short section scrolled to the top.
const TOP_HEADING: f32 = 16.0;

/// The section that owns the view of a scroll area that starts at
/// `top` and is `height` tall: the one whose heading sits at its top edge,
/// otherwise the one found by [`section_in_view`].
fn owning_section(tops: &[(Section, f32)], top: f32, height: f32, at_end: bool) -> Section {
    if !at_end
        && let Some((s, _)) = tops
            .iter()
            .rev()
            .find(|(_, t)| (*t - top).abs() <= TOP_HEADING)
    {
        return *s;
    }
    section_in_view(tops, highlight_line(top, height), at_end)
}

/// A section the user asked for (list click or request): it stays
/// highlighted until the page is scrolled away from where it landed.
#[derive(Clone, Copy)]
struct Pin {
    section: Section,
    /// Scroll offset the request lands at.
    offset: f32,
    arrived: bool,
}

/// The pin after a frame at scroll `offset`, or `None` once released.
fn settle_pin(pin: Pin, offset: f32, at_end: bool) -> Option<Pin> {
    let near = (offset - pin.offset).abs() <= 2.0 || (at_end && offset < pin.offset);
    if near {
        Some(Pin {
            arrived: true,
            ..pin
        })
    } else if pin.arrived {
        None
    } else {
        Some(pin)
    }
}

/// Where the parts of the page go for a window `width` wide.
#[derive(Debug, PartialEq)]
struct PageLayout {
    /// Left edge of the section list; `None` below [`NAV_MIN_WIDTH`].
    nav_left: Option<f32>,
    content_left: f32,
    content_width: f32,
}

fn page_layout(width: f32) -> PageLayout {
    if width >= NAV_MIN_WIDTH {
        let content_width = (width - NAV_WIDTH - NAV_GAP - 32.0).min(MAX_WIDTH);
        let total = NAV_WIDTH + NAV_GAP + content_width;
        let left = ((width - total) / 2.0).max(12.0);
        PageLayout {
            nav_left: Some(left),
            content_left: left + NAV_WIDTH + NAV_GAP,
            content_width,
        }
    } else {
        let content_width = (width - 24.0).min(MAX_WIDTH);
        PageLayout {
            nav_left: None,
            content_left: (width - content_width) / 2.0,
            content_width,
        }
    }
}

/// Width for a row's text: the card's inner width less the control and the
/// gap, so a long description wraps before it reaches the control.
fn row_text_width(inner: f32, control: f32) -> f32 {
    (inner - control - CONTROL_GAP).max(MIN_TEXT_WIDTH)
}

/// Whether the own-index group is open: the user's choice, else the default.
fn own_index_open(default_open: bool, chosen: Option<bool>) -> bool {
    chosen.unwrap_or(default_open)
}

/// The group's default, decided once per session: open when the Ferrow
/// service was not live (its settings then matter), closed when it was. A
/// service that goes away or reconnects later does not move the group under
/// the user's cursor.
fn latched_own_index_default(ctx: &egui::Context, live: bool) -> bool {
    ctx.data_mut(|d| *d.get_temp_mut_or_insert_with(own_default_id(), || !live))
}

fn own_default_id() -> egui::Id {
    egui::Id::unique("settings-own-default")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tone {
    Good,
    Caution,
    Neutral,
}

fn tone_color(p: &theme::Palette, tone: Tone) -> Color32 {
    match tone {
        Tone::Good => p.ok(),
        Tone::Caution => p.caution(),
        Tone::Neutral => p.text_tertiary,
    }
}

/// The first thing on the page: is the fast, live index on, and if not, why.
#[derive(Debug, PartialEq)]
struct FastIndexStatus {
    tone: Tone,
    on: bool,
    /// The state word next to the title: "On", "Off", "Checking".
    word: &'static str,
    line: String,
    reason: Option<String>,
    /// Asking the service again can change the answer.
    retry: bool,
}

fn fast_index_status(service_entries: Option<u64>, view: &access::View) -> FastIndexStatus {
    let off = |tone, reason: &str, retry| FastIndexStatus {
        tone,
        on: false,
        word: "Off",
        line: "This app uses its own index, not live.".to_owned(),
        reason: Some(reason.to_owned()),
        retry,
    };
    if let Some(entries) = service_entries {
        return FastIndexStatus {
            tone: Tone::Good,
            on: true,
            word: "On",
            line: format!(
                "The Ferrow service indexes every drive ({} files) and keeps it current.",
                super::group(entries as usize)
            ),
            reason: None,
            retry: false,
        };
    }
    match view {
        access::View::Checking => FastIndexStatus {
            tone: Tone::Neutral,
            on: false,
            word: "Checking",
            line: "Asking the Ferrow service…".to_owned(),
            reason: None,
            retry: false,
        },
        access::View::NoService => off(
            Tone::Caution,
            "The Ferrow service isn't installed or isn't running. Install or start it with Setup to index every drive in seconds and keep the index current.",
            true,
        ),
        access::View::TooOld => off(
            Tone::Caution,
            "The service didn't answer: it is older than this app, or busy. Retry, or update Ferrow with Setup.",
            true,
        ),
        access::View::Info(info) if !info.you_allowed => off(
            Tone::Caution,
            "The service is running but doesn't allow this account. Allow it below.",
            false,
        ),
        access::View::Info(_) => off(
            Tone::Neutral,
            "The service allows this account but isn't answering searches yet.",
            false,
        ),
    }
}

/// "today 09:12", "yesterday 18:40" or "2026-11-14 09:12", in the zone
/// `offset_secs` east of UTC.
fn checked_when(last: u64, now: u64, offset_secs: i32) -> String {
    let Some(zone) = chrono::FixedOffset::east_opt(offset_secs) else {
        return "earlier".to_owned();
    };
    let at = |secs: u64| {
        chrono::DateTime::from_timestamp(secs as i64, 0).map(|t| t.with_timezone(&zone))
    };
    let (Some(last), Some(now)) = (at(last), at(now)) else {
        return "earlier".to_owned();
    };
    let time = last.format("%H:%M");
    match (now.date_naive() - last.date_naive()).num_days() {
        0 => format!("today {time}"),
        1 => format!("yesterday {time}"),
        _ => format!("{}", last.format("%Y-%m-%d %H:%M")),
    }
}

/// The updates row: version and result on the first line, when it was
/// checked on the second. Never claims a result it does not have.
fn update_texts(
    state: &UpdateState,
    current: &ferrow_update::Version,
    last_check: Option<u64>,
    now: u64,
    offset_secs: i32,
) -> (String, String) {
    let checked = last_check.map(|t| checked_when(t, now, offset_secs));
    let with_check = |text: &str| match &checked {
        Some(when) => format!("{text} Checked {when}."),
        None => text.to_owned(),
    };
    match state {
        UpdateState::Idle => (
            format!("Ferrow {current}"),
            match &checked {
                Some(when) => format!("Last checked {when}."),
                None => "Not checked yet.".to_owned(),
            },
        ),
        UpdateState::Checking => (
            format!("Ferrow {current}"),
            "Checking for updates…".to_owned(),
        ),
        UpdateState::UpToDate => (
            format!("Ferrow {current} · Up to date"),
            checked.map_or(String::new(), |when| format!("Checked {when}.")),
        ),
        UpdateState::Available(r) => (
            format!("Ferrow {} is available", r.version),
            with_check(&format!("You have {current}.")),
        ),
        UpdateState::CheckFailed(e) => (
            format!("Ferrow {current} · Check failed"),
            format!("The check failed: {e}"),
        ),
        UpdateState::Installing => (
            "Installing the update…".to_owned(),
            "Approve the administrator prompt. Ferrow closes and reopens.".to_owned(),
        ),
        UpdateState::InstallFailed(e) => ("The update was not installed".to_owned(), e.clone()),
    }
}

/// Seconds east of UTC for the dates shown (UTC in test builds, so the
/// screenshots do not depend on the machine's zone).
fn local_offset_secs() -> i32 {
    #[cfg(not(test))]
    return chrono::Local::now().offset().local_minus_utc();
    #[cfg(test)]
    0
}

/// "Now" for the dates shown; fixed by the screenshot harness.
fn display_now() -> u64 {
    #[cfg(test)]
    {
        let fixed = TEST_NOW.load(std::sync::atomic::Ordering::Relaxed);
        if fixed != 0 {
            return fixed;
        }
    }
    unix_now()
}

#[cfg(all(test, feature = "screenshots"))]
pub(crate) fn test_set_now(secs: u64) {
    TEST_NOW.store(secs, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
static TEST_NOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What the section anchors of one frame say; the next frame reads it for the
/// highlight in the list and for the room left below the last section.
#[derive(Clone, Default)]
struct Anchors {
    tops: Vec<(Section, f32)>,
    /// Height of the last section.
    tail: f32,
    /// Top and height of the scroll area.
    top: f32,
    height: f32,
    /// Scroll offset.
    offset: f32,
    /// Whether the page is scrolled to its end.
    at_end: bool,
    pin: Option<Pin>,
}

impl Anchors {
    /// The section the list highlights.
    fn shown(&self) -> Section {
        self.pin.map_or_else(
            || owning_section(&self.tops, self.top, self.height, self.at_end),
            |p| p.section,
        )
    }
}

/// Screenshot tests: which section is in view (the page's own rule).
#[cfg(all(test, feature = "screenshots"))]
pub(crate) fn section_label_in_view(ctx: &egui::Context) -> &'static str {
    let anchors: Anchors = ctx.data(|d| d.get_temp(anchors_id())).unwrap_or_default();
    anchors.shown().label()
}

fn anchors_id() -> egui::Id {
    egui::Id::unique("settings-anchors")
}

fn own_open_id() -> egui::Id {
    egui::Id::unique("settings-own-open")
}

/// Records where each section starts and scrolls to a requested one.
struct Marks {
    tops: Vec<(Section, f32)>,
    scroll_to: Option<Section>,
    /// Scroll offset the request made this frame lands at.
    landed: Option<f32>,
    /// Offset the page had when the frame began, and the area's top.
    offset: f32,
    top: f32,
}

impl Marks {
    fn begin(&mut self, ui: &mut egui::Ui, section: Section) {
        let top = ui.cursor().top();
        self.tops.push((section, top));
        if self.scroll_to == Some(section) {
            self.scroll_to = None;
            let at = egui::Rect::from_min_size(
                egui::pos2(ui.max_rect().left(), top - 8.0),
                vec2(1.0, 1.0),
            );
            ui.scroll_to_rect(at, Some(Align::TOP));
            self.landed = Some((self.offset + top - 8.0 - self.top).max(0.0));
        }
    }
}

impl FerrowApp {
    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        self.poll_folder_pick();
        self.poll_drives();
        let ctx = ui.ctx().clone();
        let outer = ui.max_rect();
        let layout = page_layout(outer.width());

        // The title row stays in view; the sections scroll below it.
        let left = outer.left() + layout.nav_left.unwrap_or(layout.content_left);
        let title = egui::Rect::from_min_size(
            egui::pos2(left, outer.top() + 8.0),
            vec2(outer.right() - left, TITLE_HEIGHT - 8.0),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(title), |ui| {
            ui.horizontal(|ui| {
                if back_button(ui, &p).clicked() {
                    self.page = Page::Results;
                    self.focus_search = true;
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Settings")
                        .font(FontId::new(28.0, FontFamily::Name(theme::SEMIBOLD.into())))
                        .color(p.text),
                );
            });
        });
        let body_top = outer.top() + TITLE_HEIGHT;

        // The list on the left highlights the section in view (from the last
        // frame) and asks to scroll to the one clicked.
        let before: Anchors = ctx.data(|d| d.get_temp(anchors_id())).unwrap_or_default();
        let current = before.shown();
        let mut target = self
            .settings_scroll_to
            .take()
            .and_then(Section::from_target);
        if let Some(nav_left) = layout.nav_left {
            let nav = egui::Rect::from_min_max(
                egui::pos2(outer.left() + nav_left, body_top + 4.0),
                egui::pos2(outer.left() + nav_left + NAV_WIDTH, outer.bottom()),
            );
            let live = self.service_entries.is_some();
            ui.scope_builder(egui::UiBuilder::new().max_rect(nav), |ui| {
                if let Some(section) = nav_list(ui, &p, current, live) {
                    target = Some(section);
                }
            });
        }
        if target == Some(Section::OwnIndex) {
            ctx.data_mut(|d| d.insert_temp(own_open_id(), Some(true)));
        }

        let scroll = egui::Rect::from_min_max(
            egui::pos2(outer.left() + layout.content_left - 12.0, body_top),
            outer.max,
        );
        let viewport_h = scroll.height();
        let mut marks = Marks {
            tops: Vec::new(),
            scroll_to: target,
            landed: None,
            offset: before.offset,
            top: body_top,
        };
        let mut tail = before.tail;
        let mut body = ui.new_child(egui::UiBuilder::new().max_rect(scroll));
        let output = egui::ScrollArea::vertical()
            .id_salt("settings-scroll")
            .auto_shrink([false, false])
            .show(&mut body, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.set_width(layout.content_width);
                        ui.add_space(4.0);
                        self.settings_error_line(ui);
                        marks.begin(ui, Section::FastIndex);
                        self.fast_index_section(ui);
                        ui.add_space(SECTION_GAP);
                        marks.begin(ui, Section::OwnIndex);
                        self.own_index_section(ui);
                        if cfg!(windows) {
                            ui.add_space(SECTION_GAP);
                            marks.begin(ui, Section::Windows);
                            self.windows_section(ui);
                        }
                        ui.add_space(SECTION_GAP);
                        marks.begin(ui, Section::Appearance);
                        self.appearance_section(ui);
                        ui.add_space(SECTION_GAP);
                        marks.begin(ui, Section::Updates);
                        self.updates_section(ui);
                        ui.add_space(SECTION_GAP);
                        let about_top = ui.cursor().top();
                        marks.begin(ui, Section::About);
                        self.about_section(ui);
                        tail = ui.cursor().top() - about_top;
                        // Room to scroll the last section to the top.
                        ui.add_space((viewport_h - tail - 24.0).max(24.0));
                    });
                });
            });
        let offset = output.state.clamped_offset().y;
        let at_end =
            offset > 0.5 && offset + output.inner_rect.height() >= output.content_size.y - 1.0;
        if offset > 0.5 {
            scrolled_edge(ui, &p, output.inner_rect);
        }
        let pin = match (target, marks.landed) {
            (Some(section), Some(landed)) => Some(Pin {
                section,
                offset: landed,
                arrived: false,
            }),
            _ => before.pin.and_then(|pin| settle_pin(pin, offset, at_end)),
        };
        if let Some(section) = target
            && before.tail <= 0.0
        {
            // First frame of the page: the room below the last section is
            // not measured yet, so a far section could not reach the top.
            // Ask again once, next frame.
            self.settings_scroll_to = Some(section.label());
            ctx.request_repaint();
        }
        let now = Anchors {
            tops: marks.tops,
            tail,
            top: body_top,
            height: viewport_h,
            offset,
            at_end,
            pin,
        };
        // The highlight follows the scroll position; one more frame when it
        // changed, never while it holds.
        let changed = now.shown() != current || (now.tail - before.tail).abs() > 0.5;
        ctx.data_mut(|d| d.insert_temp(anchors_id(), now));
        if changed {
            ctx.request_repaint();
        }
    }

    fn settings_error_line(&self, ui: &mut egui::Ui) {
        if let Some(err) = &self.settings_error {
            ui.label(RichText::new(err).color(Color32::from_rgb(0xE8, 0x5A, 0x4F)));
            ui.add_space(8.0);
        }
    }

    /// Fast index: the status first, then who may use the service.
    fn fast_index_section(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        let ctx = ui.ctx().clone();
        // The result is polled every frame in `ui()`; this page only draws it.
        if self.access.needs_refresh() {
            let wake = ctx.clone();
            self.access.refresh(move || wake.request_repaint());
        }
        let status = fast_index_status(self.service_entries, &self.access.view);
        let view = self.access.view.clone();
        let message = self.access.message.clone();
        let input = AccessIn {
            view: &view,
            busy: self.access.busy,
            message: message.as_ref(),
            elevate: access::Access::can_elevate(),
            sandboxed: access::Access::sandboxed(),
            add: self.access.add.as_ref(),
        };
        let mut out = AccessOut {
            action: None,
            confirm: self.access.confirm_everyone,
            add: self.access.add.clone(),
            look_up: false,
        };
        let mut retry = false;
        let color = tone_color(&p, status.tone);
        status_card(ui, &p, color, |ui| {
            ui.horizontal(|ui| {
                dot(ui, color, status.on, 14.0);
                ui.label(
                    RichText::new("Fast index")
                        .font(FontId::new(18.0, FontFamily::Name(theme::SEMIBOLD.into())))
                        .color(p.text),
                );
                ui.label(
                    RichText::new(status.word)
                        .font(FontId::new(14.0, FontFamily::Name(theme::SEMIBOLD.into())))
                        .color(if status.on { color } else { p.text }),
                );
                if status.retry {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        retry = ui.add(std_button(&p, "Retry", 90.0)).clicked();
                    });
                }
            });
            ui.add_space(4.0);
            ui.label(RichText::new(&status.line).size(14.0).color(p.text));
            if let Some(reason) = &status.reason {
                ui.label(RichText::new(reason).size(12.0).color(p.text_secondary));
            }
            if matches!(input.view, access::View::Info(_)) || input.busy || input.message.is_some()
            {
                ui.add_space(12.0);
                let line = ui.cursor().top();
                ui.painter()
                    .hline(ui.max_rect().x_range(), line, Stroke::new(1.0, p.divider));
                ui.add_space(12.0);
                out = access_block(ui, &p, &input, out.confirm);
            }
        });
        self.access.confirm_everyone = out.confirm;
        self.access.add = out.add;
        if out.look_up {
            let wake = ctx.clone();
            self.access.look_up(move || wake.request_repaint());
        }
        if retry {
            self.access.retry();
        }
        if let Some(action) = out.action {
            let wake = ctx.clone();
            self.access.run(action, move || wake.request_repaint());
        }
    }

    /// Everything the service ignores while it is live, in one group that is
    /// closed then and open when the app relies on its own index.
    fn own_index_section(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        let live = self.service_entries.is_some();
        let chosen: Option<bool> = ui
            .ctx()
            .data(|d| d.get_temp::<Option<bool>>(own_open_id()))
            .flatten();
        let open = own_index_open(latched_own_index_default(ui.ctx(), live), chosen);
        let summary = format!(
            "{} · {}",
            count_text(self.settings.sources.len(), "location", "locations"),
            count_text(
                self.settings.excludes.len(),
                "excluded folder",
                "excluded folders"
            ),
        );
        let clicked = expander_header(
            ui,
            &p,
            "This app's own index",
            "Used only when the Ferrow service isn't available.",
            (!open).then_some(summary.as_str()),
            open,
        );
        if clicked {
            ui.ctx()
                .data_mut(|d| d.insert_temp(own_open_id(), Some(!open)));
        }
        if !open {
            return;
        }
        ui.add_space(CARD_GAP);
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
            setting_row(
                ui,
                &p,
                "Follow symbolic links",
                "Also index folders that links and junctions point to.",
                40.0,
                |ui| {
                    if toggle(ui, &p, &mut self.settings.follow_symlinks).changed() {
                        changed = true;
                    }
                },
            );
        });
        ui.add_space(CARD_GAP);
        card(ui, &p, |ui| {
            setting_row(
                ui,
                &p,
                "Rebuild automatically",
                "Off by default, so Ferrow does no work while idle.",
                180.0,
                |ui| {
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
                },
            );
        });
        if cfg!(windows) {
            ui.add_space(CARD_GAP);
            card(ui, &p, |ui| {
                setting_row(
                    ui,
                    &p,
                    "Index all internal drives",
                    "Every fixed drive, including drives you add later. Removable and network drives are not included.",
                    40.0,
                    |ui| {
                        if toggle(ui, &p, &mut self.settings.windows.all_fixed_drives).changed() {
                            changed = true;
                        }
                    },
                );
            });
        }
        ui.add_space(CARD_GAP);
        card(ui, &p, |ui| {
            let detail = if live {
                "Not needed while the Ferrow service is live.".to_owned()
            } else {
                match (&self.scan_status, &self.index) {
                    (Some(s), _) => s.clone(),
                    (None, Some(index)) => format!(
                        "{} entries, loaded in {}.",
                        super::group(index.len()),
                        super::fmt_ms(self.load_time)
                    ),
                    (None, None) => "No index yet.".to_owned(),
                }
            };
            setting_row(ui, &p, "Rebuild index", &detail, 120.0, |ui| {
                let can = !live && self.settings.has_locations() && self.scan_status.is_none();
                let resp = ui.add_enabled(can, accent_button(&p, "Rebuild now", 120.0, can));
                if !live && !self.settings.has_locations() {
                    resp.on_disabled_hover_text("Add a location to index first.");
                } else if resp.clicked() {
                    self.rebuild_index();
                }
            });
        });
        if changed {
            self.save_settings();
        }
    }

    fn windows_section(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        section_title(ui, &p, "Windows");
        let mut changed = false;
        let mut winsearch_changed = false;
        card(ui, &p, |ui| {
            setting_row(
                ui,
                &p,
                "Start Ferrow when you sign in",
                "Starts in the notification area, so search and the Start menu integration are ready right away. Closing the window keeps Ferrow running there.",
                40.0,
                |ui| {
                    if toggle(ui, &p, &mut self.settings.windows.start_at_sign_in).changed() {
                        changed = true;
                        platform::sync_start_at_sign_in(self.settings.windows.start_at_sign_in);
                    }
                },
            );
        });
        ui.add_space(CARD_GAP);
        card(ui, &p, |ui| {
            let current = self.settings.windows.hotkey;
            let detail = if platform::global_hotkey_unavailable() {
                format!(
                    "{} is taken by another app. Choose another shortcut.",
                    current.label()
                )
            } else {
                "Shows Ferrow from anywhere with the search box ready; press it again to put Ferrow away."
                    .to_owned()
            };
            setting_row(ui, &p, "Keyboard shortcut", &detail, 180.0, |ui| {
                egui::ComboBox::from_id_salt("global-hotkey")
                    .selected_text(current.label())
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        for choice in crate::settings::Hotkey::ALL {
                            if ui
                                .selectable_label(current == choice, choice.label())
                                .clicked()
                                && choice != current
                            {
                                self.settings.windows.hotkey = choice;
                                platform::set_global_hotkey(choice);
                                changed = true;
                            }
                        }
                    });
            });
        });
        ui.add_space(CARD_GAP);
        card(ui, &p, |ui| {
            let detail = if self.settings.windows.windows_search {
                let status = ferrow_winsearch::status();
                if let Some(error) = status.last_error {
                    format!("On, but not active: {error}")
                } else if status.attached_pid.is_some() {
                    match status.source {
                        Some(ferrow_winsearch::ipc::IndexSource::Service) => {
                            "On: Start menu searches use the Ferrow service index.".to_owned()
                        }
                        Some(ferrow_winsearch::ipc::IndexSource::Local) => {
                            "On: Start menu searches use this app's index.".to_owned()
                        }
                        None => "On: Start menu searches use Ferrow's index.".to_owned(),
                    }
                } else {
                    "On: waiting to connect to Windows Search…".to_owned()
                }
            } else {
                "Use Ferrow's index to answer file and folder searches in the Windows Start menu."
                    .to_owned()
            };
            setting_row(
                ui,
                &p,
                "Power Windows Search with Ferrow",
                &detail,
                40.0,
                |ui| {
                    if toggle(ui, &p, &mut self.settings.windows.windows_search).changed() {
                        changed = true;
                        winsearch_changed = true;
                    }
                },
            );
        });
        if changed {
            self.save_settings();
        }
        if winsearch_changed {
            self.sync_windows_search();
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
                "From \"Index all internal drives\" below",
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
            setting_row(
                ui,
                &p,
                "Theme",
                "Follow Windows, or always use light or dark.",
                3.0 * 76.0 + 2.0 * 8.0,
                |ui| {
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
                },
            );
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
            let (title, detail) = update_texts(
                &self.updater.state,
                &current,
                self.settings.last_update_check,
                display_now(),
                local_offset_secs(),
            );
            let installable = matches!(&self.updater.state, UpdateState::Available(r)
                if update::can_install() && r.has_setup);
            let download = matches!(self.updater.state, UpdateState::Available(_)) && !installable;
            let controls = 100.0
                + if installable {
                    130.0 + 8.0
                } else if download {
                    100.0 + 8.0
                } else {
                    0.0
                };
            setting_row(ui, &p, &title, &detail, controls, |ui| {
                let busy = self.updater.busy();
                if let UpdateState::Available(release) = &self.updater.state {
                    let release = release.clone();
                    if installable {
                        if ui
                            .add_enabled(!busy, accent_button(&p, "Install update", 130.0, !busy))
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
            ui.add_space(10.0);
            let auto = match UpdateChannel::Auto.resolve(&current) {
                ferrow_update::Channel::Beta => "Automatic (Beta)",
                ferrow_update::Channel::Stable => "Automatic (Stable)",
            };
            setting_row(
                ui,
                &p,
                "Update channel",
                "Stable gets tested releases. Beta also gets previews of the next version.",
                170.0,
                |ui| {
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
                },
            );
        });
    }

    /// Licence and where the data lives; the version is under Updates.
    fn about_section(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        section_title(ui, &p, "About");
        card(ui, &p, |ui| {
            row_header(
                ui,
                &p,
                "Licence",
                "Ferrow is open source under the MIT licence.",
            );
            if let Some(path) = &crate::engine::default_index_path() {
                ui.add_space(10.0);
                let folder = path.parent().unwrap_or(path);
                setting_row(
                    ui,
                    &p,
                    "Data folder",
                    &folder.display().to_string(),
                    80.0,
                    |ui| {
                        if ui
                            .add(theme::FilledButton::standard(
                                &p,
                                egui::Button::new("Open").min_size(vec2(80.0, 30.0)),
                            ))
                            .clicked()
                        {
                            platform::open(folder);
                        }
                    },
                );
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

/// Height of the fade under the scroll area's top edge.
const EDGE_FADE: f32 = 14.0;

/// The clean top edge of the scrolled content: a hairline and a short fade
/// into the page colour, so a row cut by the edge does not read as broken
/// text. Painted only while the content is scrolled, as part of the frame
/// that scrolled; it never asks for a repaint.
fn scrolled_edge(ui: &egui::Ui, p: &theme::Palette, area: egui::Rect) {
    let top = area.top();
    let fade = egui::Rect::from_min_max(
        egui::pos2(area.left(), top),
        egui::pos2(area.right(), top + EDGE_FADE),
    );
    let mut mesh = egui::Mesh::default();
    let clear = p.window.gamma_multiply(0.0);
    mesh.colored_vertex(fade.left_top(), p.window);
    mesh.colored_vertex(fade.right_top(), p.window);
    mesh.colored_vertex(fade.right_bottom(), clear);
    mesh.colored_vertex(fade.left_bottom(), clear);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    let painter = ui.painter_at(area.expand2(vec2(0.0, 1.0)));
    painter.add(egui::Shape::mesh(mesh));
    painter.hline(area.x_range(), top, egui::Stroke::new(1.0, p.divider));
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
    if !detail.is_empty() {
        ui.label(RichText::new(detail).size(12.0).color(p.text_secondary));
    }
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

// -------------------------------------------------------------------
// Fast index: who can use the service

struct AccessIn<'a> {
    view: &'a access::View,
    busy: bool,
    message: Option<&'a access::Notice>,
    /// This app can start the administrator command itself.
    elevate: bool,
    sandboxed: bool,
    add: Option<&'a access::AddAccount>,
}

struct AccessOut {
    action: Option<access::Action>,
    /// "Allow everyone" waits for its confirmation.
    confirm: bool,
    /// The "Add account" field after this frame (`None`: closed).
    add: Option<access::AddAccount>,
    /// Look the typed account up.
    look_up: bool,
}

/// "Who can use the service": whether this account is served and under which
/// rule, the actions that change it, and how the last change ended.
fn access_block(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    input: &AccessIn,
    mut confirm: bool,
) -> AccessOut {
    let (busy, elevate) = (input.busy, input.elevate);
    let mut action = None;
    let mut add = input.add.cloned();
    let mut look_up = false;
    if let access::View::Info(info) = input.view {
        ui.label(
            RichText::new("Who can use the service")
                .font(FontId::new(14.0, FontFamily::Name(theme::SEMIBOLD.into())))
                .color(p.text),
        );
        ui.add_space(6.0);
        let (title, detail) = access::summary(input.view);
        // The status above already says the app uses its own index.
        let detail = if info.you_allowed {
            detail
        } else {
            access::rule(info)
        };
        let tone = if info.you_allowed {
            Tone::Good
        } else {
            Tone::Caution
        };
        dot_row(ui, p, tone, true, &title, &detail);
        let me = info.you.id.clone();
        if let Some(users) = &info.users {
            for user in users {
                ui.add_space(8.0);
                // The name on the left takes the rest of the row, clipped
                // with an ellipsis; the full text is in the tooltip.
                let name_w = row_text_width(ui.available_width(), 90.0);
                ui.horizontal(|ui| {
                    let full = if user.name.is_empty() {
                        user.id.clone()
                    } else {
                        user.name.clone()
                    };
                    ui.scope(|ui| {
                        ui.set_width(name_w);
                        ui.add(
                            egui::Label::new(RichText::new(&full).size(13.0).color(p.text))
                                .truncate(),
                        )
                        .on_hover_text(full);
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let remove =
                            ui.add_enabled(!busy && elevate, std_button(p, "Remove", 90.0));
                        if remove.clicked() {
                            action = Some(access::Action::Remove(user.id.clone()));
                        }
                    });
                });
            }
        }
        ui.add_space(10.0);
        let allow_me = !info.you_allowed
            && info.mode != ferrow_ipc::AccessMode::Everyone
            && !me.is_empty()
            && !input.sandboxed;
        let everyone = info.mode != ferrow_ipc::AccessMode::Everyone;
        let admins = info.mode != ferrow_ipc::AccessMode::Admins;
        if confirm && !busy {
            row_header(
                ui,
                p,
                "Allow every person who signs in to this PC?",
                "They will see every file name on this PC, including other people's folders.",
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .add(accent_button(p, "Allow everyone", 140.0, true))
                    .clicked()
                {
                    action = Some(access::Action::Everyone);
                }
                if ui.add(std_button(p, "Cancel", 90.0)).clicked() {
                    confirm = false;
                }
            });
        } else if elevate {
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if allow_me && ui.add(accent_button(p, "Allow me", 110.0, !busy)).clicked() {
                        action = Some(access::Action::AllowMe(me.clone()));
                    }
                    if everyone && ui.add(std_button(p, "Allow everyone", 140.0)).clicked() {
                        confirm = true;
                    }
                    if admins
                        && ui
                            .add(std_button(p, "Administrators only", 160.0))
                            .clicked()
                    {
                        action = Some(access::Action::Admins);
                    }
                    if add.is_none() && ui.add(std_button(p, "Add account…", 150.0)).clicked() {
                        add = Some(access::AddAccount::opened());
                    }
                });
            });
        } else {
            let rows = [
                allow_me.then(|| access::Action::AllowMe(me.clone())),
                everyone.then_some(access::Action::Everyone),
                admins.then_some(access::Action::Admins),
            ];
            row_header(
                ui,
                p,
                "To change this, run in an administrator terminal:",
                "",
            );
            for command in rows
                .into_iter()
                .flatten()
                .filter_map(|a| a.terminal_command())
            {
                command_row(ui, p, &command);
            }
            if add.is_none() && ui.add(std_button(p, "Add account…", 150.0)).clicked() {
                add = Some(access::AddAccount::opened());
            }
        }
        if add.is_some() && !(confirm && !busy) {
            ui.add_space(12.0);
            add_account_editor(ui, p, &mut add, &mut look_up, &mut action, busy, elevate);
        }
    }
    if busy {
        ui.add_space(10.0);
        dot_row(
            ui,
            p,
            Tone::Caution,
            false,
            "Waiting for the administrator prompt…",
            "Approve it to continue. Over remote desktop it can be on the secure desktop or behind this window.",
        );
    }
    if let Some(notice) = input.message {
        ui.add_space(10.0);
        let (tone, filled) = match notice.outcome {
            access::Outcome::Done => (Tone::Good, true),
            // The command succeeded but this account cannot see the result.
            access::Outcome::Unconfirmed => (Tone::Neutral, true),
            // The command ran and there was nothing to do (Service health).
            access::Outcome::NoChange => (Tone::Neutral, true),
            access::Outcome::Declined => (Tone::Neutral, false),
            access::Outcome::Problem => (Tone::Caution, true),
        };
        dot_row(ui, p, tone, filled, &notice.text, "");
    }
    AccessOut {
        action,
        confirm,
        add,
        look_up,
    }
}

/// The "Add account" field: type a name or SID, look it up, then add what was
/// found. Only the looked-up id reaches the administrator command.
fn add_account_editor(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    add: &mut Option<access::AddAccount>,
    look_up: &mut bool,
    action: &mut Option<access::Action>,
    busy: bool,
    elevate: bool,
) {
    let Some(field) = add.as_mut() else { return };
    let mut close = false;
    row_header(
        ui,
        p,
        "Add an account",
        if cfg!(windows) {
            r"Type DOMAIN\user, user or a SID. Ferrow looks it up before anything changes."
        } else {
            "Type a user name or a numeric id."
        },
    );
    ui.add_space(6.0);
    let looking = field.state == access::AddState::Looking;
    let can_look = !busy && !looking && !field.text.trim().is_empty();
    ui.horizontal_wrapped(|ui| {
        let hint = if cfg!(windows) {
            r"DOMAIN\user, user or SID"
        } else {
            "user name"
        };
        let width = (ui.available_width() - 200.0).clamp(160.0, 340.0);
        let edit = egui::TextEdit::singleline(&mut field.text)
            .hint_text(hint)
            .desired_width(width)
            .margin(vec2(8.0, 6.0))
            // The default frame is transparent here; outline it so it reads
            // as an input.
            .frame(
                egui::Frame::new()
                    .fill(p.control)
                    .stroke(Stroke::new(1.0, p.control_stroke))
                    .corner_radius(CornerRadius::same(4)),
            );
        let r = ui.add_enabled(!busy, edit);
        if field.focus {
            // Just opened: the cursor goes in, and the field is shown.
            field.focus = false;
            r.request_focus();
            r.scroll_to_me(Some(Align::Center));
        }
        if r.changed() {
            field.state = access::AddState::Idle;
        }
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let label = if cfg!(windows) {
            "Look up"
        } else {
            "Show command"
        };
        let width = if cfg!(windows) { 90.0 } else { 130.0 };
        // Once something is found, "Add" is the one primary button.
        let button = if matches!(field.state, access::AddState::Found(_)) {
            std_button(p, label, width)
        } else {
            accent_button(p, label, width, can_look)
        };
        let clicked = ui.add_enabled(can_look, button).clicked();
        if (clicked || enter) && can_look {
            *look_up = true;
        }
        if ui.add(std_button(p, "Cancel", 80.0)).clicked() {
            close = true;
        }
    });
    match &field.state {
        access::AddState::Idle => {}
        access::AddState::Looking => {
            ui.add_space(8.0);
            dot_row(ui, p, Tone::Neutral, false, "Looking up the account…", "");
        }
        access::AddState::Failed(why) => {
            ui.add_space(8.0);
            dot_row(ui, p, Tone::Caution, true, why, "");
        }
        access::AddState::Found(found) => {
            ui.add_space(8.0);
            // Off Windows nothing was looked up, only the name's characters
            // were checked: the line must not look verified.
            let (verified, title) = access::found_line(found, cfg!(windows));
            let detail = if verified && found.name != found.id {
                format!("Account id: {}", found.id)
            } else {
                String::new()
            };
            let tone = if verified { Tone::Good } else { Tone::Neutral };
            dot_row(ui, p, tone, verified, &title, &detail);
            ui.add_space(6.0);
            if elevate {
                let can = !busy;
                let label = format!("Add {}", shortened(&found.name, 32));
                if ui
                    .add_enabled(can, accent_button(p, &label, 90.0, can))
                    .clicked()
                {
                    *action = Some(access::Action::Add(found.id.clone()));
                }
            } else if let Some(command) = access::add_command(found) {
                row_header(ui, p, "To add it, run in an administrator terminal:", "");
                command_row(ui, p, &command);
            }
        }
    }
    if close {
        *add = None;
    }
}

/// A terminal command with its Copy button: on one line when it fits, else
/// wrapped with Copy below it.
fn command_row(ui: &mut egui::Ui, p: &theme::Palette, command: &str) {
    let width = ui
        .painter()
        .layout_no_wrap(command.to_owned(), FontId::monospace(12.0), p.text)
        .size()
        .x;
    let label = || egui::Label::new(RichText::new(command).monospace().size(12.0).color(p.text));
    let copy = |ui: &mut egui::Ui| {
        if ui.add(std_button(p, "Copy", 70.0)).clicked() {
            platform::copy_text(command);
        }
    };
    if width + 70.0 + ui.spacing().item_spacing.x <= ui.available_width() {
        ui.horizontal(|ui| {
            ui.add(label());
            copy(ui);
        });
    } else {
        ui.add(label().wrap());
        copy(ui);
    }
}

/// `text` cut to `max` characters with an ellipsis.
fn shortened(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let head: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// A status dot (filled) or ring, `size` points wide.
fn dot(ui: &mut egui::Ui, color: Color32, filled: bool, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let r = size / 2.0 - 1.0;
    if filled {
        ui.painter().circle_filled(rect.center(), r, color);
    } else {
        ui.painter()
            .circle_stroke(rect.center(), r - 0.5, Stroke::new(2.0, color));
    }
}

/// A dot with a line of text and, under it, a smaller one.
fn dot_row(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    tone: Tone,
    filled: bool,
    title: &str,
    detail: &str,
) {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(10.0, 20.0), Sense::hover());
        let c = rect.center_top() + vec2(0.0, 10.0);
        let color = tone_color(p, tone);
        if filled {
            ui.painter().circle_filled(c, 4.5, color);
        } else {
            ui.painter().circle_stroke(c, 4.0, Stroke::new(2.0, color));
        }
        ui.vertical(|ui| {
            row_header(ui, p, title, detail);
        });
    });
}

// -------------------------------------------------------------------
// Layout pieces

/// The card of the first section: the state colour runs down its left edge.
fn status_card(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    color: Color32,
    add: impl FnOnce(&mut egui::Ui),
) {
    let shown = egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.card_stroke))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin {
            left: 20,
            right: 16,
            top: 14,
            bottom: 14,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    let rect = shown.response.rect;
    let bar = egui::Rect::from_min_size(rect.min, vec2(4.0, rect.height()));
    ui.painter().rect_filled(
        bar,
        CornerRadius {
            nw: 6,
            sw: 6,
            ne: 0,
            se: 0,
        },
        color,
    );
}

/// The section list: the section in view is marked; returns the one clicked.
fn nav_list(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    current: Section,
    live: bool,
) -> Option<Section> {
    let mut clicked = None;
    ui.spacing_mut().item_spacing.y = 2.0;
    for section in Section::ALL.into_iter().filter(|s| s.shown()) {
        let (rect, resp) = ui.allocate_exact_size(vec2(NAV_WIDTH, 36.0), Sense::click());
        let selected = section == current;
        if selected || resp.hovered() {
            let fill = if selected {
                p.row_selected
            } else {
                p.row_hover
            };
            ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
        }
        if selected {
            let bar =
                egui::Rect::from_center_size(rect.left_center() + vec2(2.0, 0.0), vec2(3.0, 16.0));
            ui.painter()
                .rect_filled(bar, CornerRadius::same(2), p.accent_text);
        }
        let font = if selected {
            FontId::new(14.0, FontFamily::Name(theme::SEMIBOLD.into()))
        } else {
            FontId::proportional(14.0)
        };
        let text_at = rect.left_center() + vec2(16.0, 0.0);
        let galley = ui
            .painter()
            .layout_no_wrap(section.label().to_owned(), font, p.text);
        let width = galley.size().x;
        ui.painter().galley(
            egui::pos2(text_at.x, rect.center().y - galley.size().y / 2.0),
            galley,
            p.text,
        );
        if section == Section::FastIndex {
            // The state stays visible while the page is scrolled away.
            let c = egui::pos2(text_at.x + width + 12.0, rect.center().y);
            let color = if live { p.ok() } else { p.caution() };
            if live {
                ui.painter().circle_filled(c, 4.0, color);
            } else {
                ui.painter().circle_stroke(c, 3.5, Stroke::new(1.5, color));
            }
        }
        if resp.clicked() {
            clicked = Some(section);
        }
    }
    clicked
}

/// The header of a group that opens and closes. Returns true when clicked.
fn expander_header(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    title: &str,
    note: &str,
    summary: Option<&str>,
    open: bool,
) -> bool {
    let width = ui.available_width();
    let text_w = width - 32.0 - 44.0;
    let layout = |ui: &egui::Ui, text: &str, font: FontId, color: Color32| {
        let job = egui::text::LayoutJob::simple(text.to_owned(), font, color, text_w);
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let t = layout(
        ui,
        title,
        FontId::new(14.0, FontFamily::Name(theme::SEMIBOLD.into())),
        p.text,
    );
    let n = layout(ui, note, FontId::proportional(12.0), p.text_secondary);
    let s = summary.map(|s| layout(ui, s, FontId::proportional(12.0), p.text_tertiary));
    let height =
        14.0 + t.size().y + 2.0 + n.size().y + s.as_ref().map_or(0.0, |g| 2.0 + g.size().y) + 14.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(6),
        p.card,
        Stroke::new(1.0, p.card_stroke),
        StrokeKind::Inside,
    );
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(6), p.row_hover);
    }
    let mut y = rect.top() + 14.0;
    for (galley, color) in [Some((t, p.text)), Some((n, p.text_secondary))]
        .into_iter()
        .flatten()
        .chain(s.map(|g| (g, p.text_tertiary)))
    {
        let h = galley.size().y;
        painter.galley(egui::pos2(rect.left() + 16.0, y), galley, color);
        y += h + 2.0;
    }
    // The chevron points down when open, right when closed.
    let c = egui::pos2(rect.right() - 24.0, rect.center().y);
    let pts = if open {
        [vec2(-5.0, -2.5), vec2(0.0, 2.5), vec2(5.0, -2.5)]
    } else {
        [vec2(-2.5, -5.0), vec2(2.5, 0.0), vec2(-2.5, 5.0)]
    };
    painter.add(egui::Shape::line(
        pts.iter().map(|d| c + *d).collect(),
        Stroke::new(1.5, p.text_secondary),
    ));
    resp.clicked()
}

fn count_text(n: usize, one: &str, many: &str) -> String {
    match n {
        0 => format!("no {many}"),
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    }
}

/// One setting: text on the left that wraps before it reaches the control,
/// the control (`control_width` wide) on the right.
fn setting_row(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    title: &str,
    detail: &str,
    control_width: f32,
    control: impl FnOnce(&mut egui::Ui),
) {
    let text_w = row_text_width(ui.available_width(), control_width);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(text_w);
            row_header(ui, p, title, detail);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrow_ipc::{AccessInfo, AccessMode, AccessUser};

    fn info(mode: AccessMode, allowed: bool) -> access::View {
        access::View::Info(AccessInfo {
            mode,
            you_allowed: allowed,
            you: AccessUser {
                id: "S-1-5-21-1-2-3-1001".into(),
                name: String::new(),
            },
            users: None,
        })
    }

    #[test]
    fn own_index_is_closed_while_the_service_is_live_and_open_when_it_is_not() {
        let ctx = egui::Context::default();
        assert!(!own_index_open(latched_own_index_default(&ctx, true), None));
        let fresh = egui::Context::default();
        assert!(own_index_open(
            latched_own_index_default(&fresh, false),
            None
        ));
        // The user's choice wins either way.
        assert!(own_index_open(false, Some(true)));
        assert!(!own_index_open(true, Some(false)));
    }

    #[test]
    fn the_default_is_decided_once_and_a_reconnect_does_not_move_it() {
        // Opened while the service was live: closed, and it stays closed
        // when the service goes away and comes back.
        let ctx = egui::Context::default();
        assert!(!latched_own_index_default(&ctx, true));
        assert!(!latched_own_index_default(&ctx, false));
        assert!(!latched_own_index_default(&ctx, true));
        // Opened while it was not: open, and it stays open when it connects.
        let other = egui::Context::default();
        assert!(latched_own_index_default(&other, false));
        assert!(latched_own_index_default(&other, true));
    }

    #[test]
    fn a_long_account_name_is_cut_with_an_ellipsis_for_the_button() {
        assert_eq!(shortened(r"ALEX-PC\Sam", 32), r"ALEX-PC\Sam");
        let long = "x".repeat(40);
        let cut = shortened(&long, 32);
        assert_eq!(cut.chars().count(), 32);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn the_status_says_on_with_the_file_count_or_off_with_the_reason() {
        let live = fast_index_status(Some(18_422_907), &access::View::Checking);
        assert!(live.on && live.tone == Tone::Good && live.word == "On");
        assert!(live.line.contains("18,422,907 files") && live.line.contains("every drive"));
        assert!(live.reason.is_none() && !live.retry);

        let reason = |view: &access::View| {
            let s = fast_index_status(None, view);
            assert!(!s.on, "{view:?}");
            s
        };
        let none = reason(&access::View::NoService);
        assert!(none.line.contains("own index, not live"));
        assert!(
            none.reason
                .as_deref()
                .unwrap()
                .contains("isn't installed or isn't running")
        );
        assert!(none.retry);
        let old = reason(&access::View::TooOld);
        assert!(old.reason.as_deref().unwrap().contains("older"));
        let refused = reason(&info(AccessMode::Admins, false));
        assert!(
            refused
                .reason
                .as_deref()
                .unwrap()
                .contains("doesn't allow this account")
        );
        assert_eq!(refused.tone, Tone::Caution);
        assert_eq!(reason(&access::View::Checking).tone, Tone::Neutral);
        // The service live wins over a stale access answer.
        assert!(fast_index_status(Some(1), &access::View::NoService).on);
    }

    #[test]
    fn requests_scroll_to_the_section_they_name() {
        assert_eq!(Section::from_target("Fast index"), Some(Section::FastIndex));
        // "Service access" lives in the fast index section.
        assert_eq!(
            Section::from_target("Service access"),
            Some(Section::FastIndex)
        );
        assert_eq!(Section::from_target("Own index"), Some(Section::OwnIndex));
        assert_eq!(Section::from_target("updates"), Some(Section::Updates));
        assert_eq!(Section::from_target("Nope"), None);
        assert_eq!(Section::from_target("Windows").is_some(), cfg!(windows));
    }

    #[test]
    fn the_section_in_view_is_the_last_one_that_started_above_the_line() {
        let tops = [
            (Section::FastIndex, -300.0),
            (Section::OwnIndex, -40.0),
            (Section::Appearance, 80.0),
            (Section::Updates, 500.0),
        ];
        assert_eq!(section_in_view(&tops, 56.0, false), Section::OwnIndex);
        assert_eq!(section_in_view(&tops, 100.0, false), Section::Appearance);
        // Before any section has started, the first one.
        let below = [(Section::FastIndex, 120.0), (Section::OwnIndex, 400.0)];
        assert_eq!(section_in_view(&below, 56.0, false), Section::FastIndex);
        assert_eq!(section_in_view(&[], 56.0, false), Section::FastIndex);
    }

    #[test]
    fn a_section_takes_the_highlight_once_its_heading_is_in_the_upper_part() {
        // The owner's picture of beta.17: a 968 point tall scroll area at
        // y 92 with the Windows heading 370 points below its top (38 %) and
        // the tail of the own index above it. Windows owns most of the view.
        let tops = [
            (Section::FastIndex, -900.0),
            (Section::OwnIndex, -520.0),
            (Section::Windows, 462.0),
            (Section::Appearance, 726.0),
            (Section::Updates, 844.0),
        ];
        let line = highlight_line(92.0, 968.0);
        assert_eq!(section_in_view(&tops, line, false), Section::Windows);
        // A heading still in the lower part of the view has not taken it.
        let lower = [(Section::OwnIndex, -520.0), (Section::Windows, 600.0)];
        assert_eq!(section_in_view(&lower, line, false), Section::OwnIndex);
        // The line scales with the view, so a short window works the same.
        assert!(highlight_line(52.0, 300.0) < 52.0 + 300.0 / 2.0);
        assert!(highlight_line(52.0, 300.0) > 52.0 + 300.0 / 3.0 - 1.0);
    }

    #[test]
    fn a_short_section_at_the_top_owns_the_view_even_if_the_next_heading_is_above_the_line() {
        // Scroll area at y 92, 968 tall (line at 479). Appearance was clicked
        // and sits 8 points below the top; Updates starts 118 points later,
        // well above the 40 % line.
        let tops = [
            (Section::Windows, -300.0),
            (Section::Appearance, 100.0),
            (Section::Updates, 218.0),
            (Section::About, 600.0),
        ];
        assert_eq!(
            owning_section(&tops, 92.0, 968.0, false),
            Section::Appearance
        );
        // Still the one just scrolled 12 points past the top.
        let past = [(Section::Appearance, 80.0), (Section::Updates, 198.0)];
        assert_eq!(
            owning_section(&past, 92.0, 968.0, false),
            Section::Appearance
        );
        // A heading 40 points down does not own the top: the line rule runs.
        let low = [(Section::Windows, -300.0), (Section::Appearance, 132.0)];
        assert_eq!(
            owning_section(&low, 92.0, 968.0, false),
            Section::Appearance
        );
        let apart = [(Section::Windows, -300.0), (Section::Appearance, 700.0)];
        assert_eq!(owning_section(&apart, 92.0, 968.0, false), Section::Windows);
        // At the end the last section wins.
        assert_eq!(owning_section(&tops, 92.0, 968.0, true), Section::About);
    }

    #[test]
    fn a_clicked_section_stays_until_the_page_is_scrolled_away() {
        let pin = Pin {
            section: Section::Updates,
            offset: 800.0,
            arrived: false,
        };
        // On the way (an animated scroll): held, not yet arrived.
        let moving = settle_pin(pin, 500.0, false).expect("held on the way");
        assert!(!moving.arrived);
        let landed = settle_pin(moving, 800.0, false).expect("held on arrival");
        assert!(landed.arrived);
        // A wheel step away releases it.
        assert!(settle_pin(landed, 860.0, false).is_none());
        // A page that cannot scroll that far counts as arrived at its end.
        let far = Pin {
            offset: 2000.0,
            ..pin
        };
        assert!(settle_pin(far, 1500.0, true).is_some_and(|p| p.arrived));
    }

    #[test]
    fn the_last_section_is_highlighted_when_scrolled_to_the_end() {
        // About is too short to reach the line at the end of the page.
        let tops = [(Section::Updates, -200.0), (Section::About, 500.0)];
        let line = highlight_line(92.0, 600.0);
        assert_eq!(section_in_view(&tops, line, false), Section::Updates);
        assert_eq!(section_in_view(&tops, line, true), Section::About);
    }

    #[test]
    fn the_section_list_stands_left_from_900_points_and_never_below() {
        let narrow = page_layout(899.0);
        assert_eq!(narrow.nav_left, None);
        assert!(narrow.content_width <= 899.0 - 24.0);
        let wide = page_layout(900.0);
        let nav = wide.nav_left.expect("a list at 900");
        assert!(nav >= 12.0 && wide.content_left >= nav + NAV_WIDTH);
        assert!(wide.content_left + wide.content_width <= 900.0);
        let huge = page_layout(2400.0);
        assert!(huge.content_width <= MAX_WIDTH);
        assert!(huge.content_left + huge.content_width <= 2400.0);
    }

    #[test]
    fn a_rows_text_leaves_room_for_its_control() {
        assert_eq!(row_text_width(500.0, 180.0), 500.0 - 180.0 - CONTROL_GAP);
        // The text is never squeezed to nothing; the control may then wrap.
        assert_eq!(row_text_width(200.0, 180.0), MIN_TEXT_WIDTH);
        assert!(row_text_width(500.0, 40.0) > row_text_width(500.0, 180.0));
    }

    #[test]
    fn the_last_check_reads_as_today_yesterday_or_a_date() {
        const DAY: u64 = 86_400;
        // 2026-11-17 12:00 UTC
        let now = 1_794_916_800;
        assert_eq!(checked_when(now - 3 * 3600, now, 0), "today 09:00");
        assert_eq!(checked_when(now - DAY, now, 0), "yesterday 12:00");
        assert_eq!(checked_when(now - 5 * DAY, now, 0), "2026-11-12 12:00");
        // In UTC+9 it is 21:00 on the 17th; 3 h earlier was today 18:00.
        assert_eq!(checked_when(now - 3 * 3600, now, 9 * 3600), "today 18:00");
        // 23 h earlier, across midnight: yesterday although under a day ago.
        assert_eq!(
            checked_when(now - 11 * 3600, now + 12 * 3600, 0),
            "yesterday 01:00"
        );
    }

    #[test]
    fn the_update_line_shows_the_version_result_and_check_time_once() {
        let v = ferrow_update::Version::parse("0.1.0-beta.16").unwrap();
        let now = 1_794_916_800;
        let last = Some(now - 3 * 3600);
        let (title, detail) = update_texts(&UpdateState::UpToDate, &v, last, now, 0);
        assert_eq!(title, "Ferrow 0.1.0-beta.16 · Up to date");
        assert_eq!(detail, "Checked today 09:00.");
        // An automatic check ran before this session: never "not checked".
        let (title, detail) = update_texts(&UpdateState::Idle, &v, last, now, 0);
        assert_eq!(title, "Ferrow 0.1.0-beta.16");
        assert_eq!(detail, "Last checked today 09:00.");
        assert!(!detail.contains("session"));
        assert_eq!(
            update_texts(&UpdateState::Idle, &v, None, now, 0).1,
            "Not checked yet."
        );
        let (title, _) = update_texts(&UpdateState::Checking, &v, last, now, 0);
        assert_eq!(title, "Ferrow 0.1.0-beta.16");
    }
}
