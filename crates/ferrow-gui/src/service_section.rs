//! Settings → Service health: the card drawn from `service::Snapshot`.
//! The state comes first, in one line; details, volumes and events follow.

use std::sync::atomic::Ordering::Relaxed;

use ferrow_ipc::health::{self, Health, VolumeHealth};

use super::*;
use crate::service::{self, Can, Level, Snapshot, Verb};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn tone(level: Level) -> Tone {
    match level {
        Level::Good => Tone::Good,
        Level::Caution => Tone::Caution,
        Level::Neutral => Tone::Neutral,
    }
}

/// `today 09:12` for a time from the service's clock (Unix milliseconds).
fn when(at_ms: u64) -> String {
    checked_when(at_ms / 1000, display_now(), local_offset_secs())
}

/// The label and value rows of the details card, with the warning level of a
/// value that needs a look.
fn details(h: &Health, scm: &service::Scm) -> Vec<(&'static str, String, Option<Level>)> {
    let mut rows = vec![(
        "Service version",
        if h.version == APP_VERSION {
            h.version.clone()
        } else {
            format!(
                "{} (this app is {APP_VERSION}; update with Setup)",
                h.version
            )
        },
        (h.version != APP_VERSION).then_some(Level::Caution),
    )];
    let since = (h.uptime_s >= 60).then(|| format!(" (since {})", when(h.started_ms)));
    rows.push((
        "Running for",
        format!(
            "{}{}",
            service::uptime_text(h.uptime_s),
            since.unwrap_or_default()
        ),
        None,
    ));
    let memory = match h.private_bytes {
        Some(private) => format!(
            "{} in use, {} private",
            service::memory_text(h.working_set),
            service::memory_text(private)
        ),
        None => format!("{} in use", service::memory_text(h.working_set)),
    };
    rows.push(("Memory", memory, None));
    rows.push(("Items", super::super::group(h.entries as usize), None));
    rows.push(match &h.last_save {
        None => ("Last save", "not saved yet".to_owned(), None),
        Some(s) if s.ok => (
            "Last save",
            format!(
                "{}, ok, {}",
                when(s.at_ms),
                health::format_duration(s.took_ms)
            ),
            None,
        ),
        Some(s) => (
            "Last save",
            format!("{}, failed: {}", when(s.at_ms), s.error),
            Some(Level::Caution),
        ),
    });
    let access = match h.access.as_str() {
        "admins" => "administrators only",
        "everyone" => "everyone who signs in to this PC",
        "listed" => "administrators and the listed accounts",
        other => other,
    };
    rows.push(("Who can use it", access.to_owned(), None));
    if let Some(start) = scm.start_text() {
        rows.push((
            "Starts",
            start.to_owned(),
            (scm.start_type == Some(4)).then_some(Level::Caution),
        ));
    }
    rows
}

/// A volume's name, its state and what the service knows about it.
fn volume_lines(v: &VolumeHealth) -> (String, Level, String, String) {
    let (mode, level) = match v.mode.as_str() {
        health::MODE_LIVE => ("live".to_owned(), Level::Good),
        health::MODE_WALK => ("directory walk, not live".to_owned(), Level::Neutral),
        health::MODE_UNAVAILABLE if v.reason.is_empty() => {
            ("live updates unavailable".to_owned(), Level::Caution)
        }
        health::MODE_UNAVAILABLE => (
            format!("live updates unavailable: {}", v.reason),
            Level::Caution,
        ),
        other => (other.to_owned(), Level::Neutral),
    };
    let mut parts = Vec::new();
    if let Some(n) = v.entries {
        parts.push(format!("{} items", service::items_text(n)));
    }
    if v.journal != health::JOURNAL_NONE && !v.journal.is_empty() {
        let detail = if v.journal_detail.is_empty() {
            String::new()
        } else {
            format!(": {}", v.journal_detail)
        };
        parts.push(format!("journal {}{detail}", v.journal));
    }
    if let Some(scan) = &v.last_scan {
        let why = if scan.reason.is_empty() {
            String::new()
        } else {
            format!(", {}", scan.reason)
        };
        parts.push(format!(
            "last scan {} ({}{why})",
            when(scan.at_ms),
            health::format_duration(scan.took_ms)
        ));
    }
    if let Some(r) = &v.rescan_pending {
        let why = if r.reason.is_empty() {
            String::new()
        } else {
            format!(" ({})", r.reason)
        };
        parts.push(format!("rescan pending{why}"));
    }
    (v.root.clone(), level, mode, parts.join(" · "))
}

struct HealthIn<'a> {
    snap: Option<&'a Snapshot>,
    /// The running action, and whether its administrator prompt is open.
    busy: Option<bool>,
    notice: Option<&'a access::Notice>,
    elevate: bool,
    confirm_repair: bool,
    events_open: bool,
}

#[derive(Default)]
struct HealthOut {
    run: Option<Verb>,
    confirm_repair: bool,
    events_open: bool,
    copy: bool,
}

impl FerrowApp {
    /// Service health: the service's state in one line, what is wrong with it,
    /// and the buttons that fix it. Reads only what the service thread has
    /// already delivered (`ServiceHealth::poll`); nothing here waits.
    pub(super) fn service_health_section(&mut self, ui: &mut egui::Ui) {
        let p = self.palette;
        let ctx = ui.ctx().clone();
        let snap = self.service_health.snap.take();
        let out = {
            let input = HealthIn {
                snap: snap.as_ref(),
                busy: self
                    .service_health
                    .busy
                    .as_ref()
                    .map(|(_, p)| p.load(Relaxed)),
                notice: self.service_health.notice.as_ref(),
                elevate: access::Access::can_elevate(),
                confirm_repair: self.service_health.confirm_repair,
                events_open: self.service_health.events_open,
            };
            section_title(ui, &p, "Service health");
            health_ui(ui, &p, &input)
        };
        self.service_health.confirm_repair = out.confirm_repair;
        self.service_health.events_open = out.events_open;
        if out.copy
            && let Some(snap) = &snap
        {
            let os = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
            platform::copy_text(&service::diagnostics(snap, APP_VERSION, &os));
            self.service_health.notice = Some(access::Notice::new(
                access::Outcome::Done,
                "Diagnostics copied to the clipboard.",
            ));
        }
        self.service_health.snap = snap;
        if let Some(verb) = out.run {
            let wake = ctx.clone();
            self.service_health
                .run(verb, move || wake.request_repaint());
        }
    }
}

fn headline(ui: &mut egui::Ui, p: &theme::Palette, s: &service::Summary, color: Color32) {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(14.0, 24.0), Sense::hover());
        let c = rect.center_top() + vec2(0.0, 12.0);
        if s.filled {
            ui.painter().circle_filled(c, 6.0, color);
        } else {
            ui.painter().circle_stroke(c, 5.5, Stroke::new(2.0, color));
        }
        ui.vertical(|ui| {
            let mut job = egui::text::LayoutJob::default();
            job.append(
                &s.head,
                0.0,
                egui::TextFormat::simple(
                    FontId::new(17.0, FontFamily::Name(theme::SEMIBOLD.into())),
                    color,
                ),
            );
            if !s.rest.is_empty() {
                job.append(
                    &format!(" · {}", s.rest),
                    0.0,
                    egui::TextFormat::simple(FontId::proportional(14.0), p.text),
                );
            }
            ui.label(job);
            if !s.hint.is_empty() {
                ui.label(RichText::new(&s.hint).size(12.0).color(p.text_secondary));
            }
        });
    });
}

fn health_ui(ui: &mut egui::Ui, p: &theme::Palette, i: &HealthIn) -> HealthOut {
    let mut out = HealthOut {
        confirm_repair: i.confirm_repair,
        events_open: i.events_open,
        ..HealthOut::default()
    };
    let s = service::summary(i.snap, APP_VERSION);
    let color = tone_color(p, tone(s.level));
    status_card(ui, p, color, |ui| {
        headline(ui, p, &s, color);
        ui.add_space(12.0);
        let line = ui.cursor().top();
        ui.painter()
            .hline(ui.max_rect().x_range(), line, Stroke::new(1.0, p.divider));
        ui.add_space(12.0);
        action_block(ui, p, i, &mut out);
    });
    let Some(snap) = i.snap else { return out };
    let Some(h) = snap.report() else { return out };
    ui.add_space(CARD_GAP);
    card(ui, p, |ui| {
        for (label, value, level) in details(h, &snap.scm) {
            ui.horizontal_top(|ui| {
                ui.add_sized(
                    vec2(130.0, 18.0),
                    egui::Label::new(RichText::new(label).size(13.0).color(p.text_secondary)),
                );
                let color = level.map_or(p.text, |l| tone_color(p, tone(l)));
                ui.add(egui::Label::new(RichText::new(value).size(13.0).color(color)).wrap());
            });
            ui.add_space(2.0);
        }
    });
    if !h.volumes.is_empty() {
        ui.add_space(CARD_GAP);
        card(ui, p, |ui| {
            ui.label(
                RichText::new("Volumes")
                    .font(FontId::new(14.0, FontFamily::Name(theme::SEMIBOLD.into())))
                    .color(p.text),
            );
            for v in &h.volumes {
                ui.add_space(8.0);
                let (root, level, mode, detail) = volume_lines(v);
                ui.horizontal_top(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 20.0), Sense::hover());
                    let c = rect.center_top() + vec2(0.0, 10.0);
                    let color = tone_color(p, tone(level));
                    if level == Level::Neutral {
                        ui.painter().circle_stroke(c, 4.0, Stroke::new(2.0, color));
                    } else {
                        ui.painter().circle_filled(c, 4.5, color);
                    }
                    ui.vertical(|ui| {
                        let mut job = egui::text::LayoutJob::default();
                        job.append(
                            &root,
                            0.0,
                            egui::TextFormat::simple(
                                FontId::new(14.0, FontFamily::Name(theme::SEMIBOLD.into())),
                                p.text,
                            ),
                        );
                        let mode_color = if level == Level::Caution {
                            color
                        } else {
                            p.text_secondary
                        };
                        job.append(
                            &format!("  {mode}"),
                            0.0,
                            egui::TextFormat::simple(FontId::proportional(13.0), mode_color),
                        );
                        ui.label(job);
                        if !detail.is_empty() {
                            ui.label(RichText::new(detail).size(12.0).color(p.text_secondary));
                        }
                    });
                });
            }
        });
    }
    events_card(ui, p, h, &mut out);
    out
}

fn events_card(ui: &mut egui::Ui, p: &theme::Palette, h: &Health, out: &mut HealthOut) {
    if h.events.is_empty() {
        return;
    }
    ui.add_space(CARD_GAP);
    card(ui, p, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Recent events")
                    .font(FontId::new(14.0, FontFamily::Name(theme::SEMIBOLD.into())))
                    .color(p.text),
            );
            if h.events.len() > 5 {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = if out.events_open {
                        "Show fewer".to_owned()
                    } else {
                        format!("Show all {}", h.events.len())
                    };
                    if ui.add(std_button(p, &label, 120.0)).clicked() {
                        out.events_open = !out.events_open;
                    }
                });
            }
        });
        for e in service::events_shown(&h.events, out.events_open) {
            ui.add_space(6.0);
            let color = match e.level.as_str() {
                health::LEVEL_ERROR => p.error(),
                health::LEVEL_WARN => p.caution(),
                _ => p.text,
            };
            ui.horizontal_top(|ui| {
                ui.add_sized(
                    vec2(128.0, 18.0),
                    egui::Label::new(
                        RichText::new(when(e.at_ms))
                            .size(12.0)
                            .color(p.text_tertiary),
                    ),
                );
                ui.add(egui::Label::new(RichText::new(&e.text).size(13.0).color(color)).wrap());
            });
        }
    });
}

/// Buttons (disabled with their reason), the repair confirmation, the
/// terminal commands where this copy cannot elevate, and the last outcome.
fn action_block(ui: &mut egui::Ui, p: &theme::Palette, i: &HealthIn, out: &mut HealthOut) {
    let acts = service::actions(i.snap, i.elevate, i.busy.is_some());
    let button = |ui: &mut egui::Ui, label: &str, width: f32, can: Can, accent: bool| match can {
        Can::Hidden => false,
        Can::No(reason) => {
            ui.add_enabled(false, std_button(p, label, width))
                .on_disabled_hover_text(reason);
            false
        }
        Can::Yes if accent => ui.add(accent_button(p, label, width, true)).clicked(),
        Can::Yes => ui.add(std_button(p, label, width)).clicked(),
    };
    ui.horizontal_wrapped(|ui| {
        if button(ui, "Start", 90.0, acts.start, true) {
            out.run = Some(Verb::Start);
        }
        if button(ui, "Restart", 100.0, acts.restart, false) {
            out.run = Some(Verb::Restart);
        }
        if button(ui, "Rescan now", 120.0, acts.rescan, false) {
            out.run = Some(Verb::Rescan);
        }
        if button(ui, "Repair…", 100.0, acts.repair, false) {
            out.confirm_repair = true;
        }
        let diag = ui.add_enabled(i.snap.is_some(), std_button(p, "Copy diagnostics", 150.0));
        out.copy |= diag.clicked();
    });
    // Why a button is off, visible without hovering (remote desktops).
    if i.snap.is_some() && i.busy.is_none() {
        for (label, can) in [
            ("Restart", acts.restart),
            ("Rescan now", acts.rescan),
            ("Repair", acts.repair),
        ] {
            if let Can::No(reason) = can
                && reason != service::NEEDS_INSTALLED
            {
                ui.label(
                    RichText::new(format!("{label}: {reason}"))
                        .size(12.0)
                        .color(p.text_secondary),
                );
            }
        }
    }
    if out.confirm_repair && i.busy.is_none() {
        ui.add_space(12.0);
        row_header(
            ui,
            p,
            "Repair the service?",
            "It re-registers the service, checks its folder, moves a damaged index aside (nothing is deleted) and restarts it. Ferrow asks for administrator approval.",
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.add(accent_button(p, "Repair", 100.0, true)).clicked() {
                out.run = Some(Verb::Repair);
            }
            if ui.add(std_button(p, "Cancel", 90.0)).clicked() {
                out.confirm_repair = false;
            }
        });
    }
    if !i.elevate && i.snap.is_some() {
        ui.add_space(10.0);
        row_header(ui, p, "To do this from a terminal, as administrator:", "");
        for (can, verb) in [
            (acts.start, Verb::Start),
            (acts.restart, Verb::Restart),
            (acts.repair, Verb::Repair),
        ] {
            if can != Can::Hidden {
                command_row(ui, p, &verb.terminal_command());
            }
        }
    }
    if let Some(prompt) = i.busy {
        ui.add_space(10.0);
        let (title, detail) = if prompt {
            (
                "Waiting for the administrator prompt…",
                "Approve it to continue. Over remote desktop it can be on the secure desktop or behind this window.",
            )
        } else {
            ("Waiting for the service…", "It can take up to 30 seconds.")
        };
        dot_row(ui, p, Tone::Caution, false, title, detail);
    }
    if let Some(notice) = i.notice {
        ui.add_space(10.0);
        let (tone, filled) = notice_look(notice.outcome);
        dot_row(ui, p, tone, filled, &notice.text, "");
    }
}
