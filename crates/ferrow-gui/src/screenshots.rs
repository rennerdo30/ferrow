//! Headless screenshots of the real app (Pair Desk FR-28).
//!
//! Renders every reachable screen of the real [`FerrowApp`] to a PNG file
//! with `egui_kittest` (wgpu, offscreen): no window is opened, no input is
//! sent to the desktop. For human review and before/after checks of UI
//! changes; nothing is compared pixel by pixel.
//!
//! Run (ignored by default so the normal test run stays fast):
//!
//! ```text
//! cargo test -p ferrow-gui --bin ferrow-gui screenshots -- --ignored --nocapture
//! ```
//!
//! `FERROW_SCREENSHOT_DIR` chooses the output folder; without it a folder
//! under the system temp directory is used and its path is printed.
//!
//! Isolation (nothing here touches the user's machine state):
//! - `engine::test_data_dir` is the only data location in test builds: it
//!   points at a scratch folder inside the output folder, which is removed
//!   at the end. Without it `default_index_path()` is `None`, so settings
//!   and index can never fall through to the real `%LOCALAPPDATA%`.
//! - The engine is `Engine::isolated`: no service connect or probe, no index
//!   file; searches run over an in-memory fixture index and the test injects
//!   the events the real worker would send.
//! - The app is built through `FerrowApp::assemble`, so no tray, hotkey,
//!   sign-in registration, Windows Search controller, single-instance or
//!   setup-exit listener is started. The update check never starts
//!   (`updater.opened` is set before the first frame).
//! - Right-click and double-click on a result row are never sent: on Windows
//!   they open the real Explorer shell menu / the file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Pos2, Vec2, pos2, vec2};
use egui_kittest::Harness;
use ferrow_index::{ColumnSet, EntryMetadata, IndexBuilder, flags};

use super::*;
use crate::access;
use crate::engine::{self, Event};
use crate::settings::{AutoRebuild, SortChoice, Source, SourceKind, ThemeChoice};
use crate::update::UpdateState;

/// Fixed "now" of the fixture: 2026-11-17 12:00 UTC.
const NOW: i64 = 1_794_916_800;
const WIDE: Vec2 = vec2(1040.0, 680.0);
const NARROW: Vec2 = vec2(560.0, 360.0);
/// The owner's window (logical points).
const OWNER: Vec2 = vec2(1570.0, 1060.0);

// -------------------------------------------------------------------
// Fixture

struct Fx {
    b: IndexBuilder,
    seed: u64,
}

impl Fx {
    fn next(&mut self) -> u64 {
        self.seed = self
            .seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.seed >> 33
    }

    fn meta(&mut self, size: u64, days_ago: u64) -> EntryMetadata {
        let secs = NOW - (days_ago * 86_400) as i64 - (self.next() % 40_000) as i64;
        EntryMetadata {
            size: Some(size),
            modified: Some(secs * 10_000_000),
            ..Default::default()
        }
    }

    fn root(&mut self, path: &str) -> u32 {
        self.b.add_root(path.as_bytes(), flags::DIR)
    }

    fn dir(&mut self, parent: u32, name: &str, days_ago: u64) -> u32 {
        let meta = self.meta(0, days_ago);
        self.b
            .add_with_metadata(parent, name.as_bytes(), flags::DIR, meta)
    }

    fn file(&mut self, parent: u32, name: &str, kb: u64, days_ago: u64) {
        let size = kb * 1024 + self.next() % 1024;
        let meta = self.meta(size, days_ago);
        self.b.add_with_metadata(parent, name.as_bytes(), 0, meta);
    }
}

/// A small, realistic home folder and project drive: about 1,900 entries,
/// many "report" files, long paths, Japanese names. Same output every run.
fn fixture_index() -> Arc<ferrow_index::Index> {
    let mut fx = Fx {
        b: IndexBuilder::with_columns(ColumnSet::DEFAULT),
        seed: 0x5EED_F0E7,
    };
    let home = fx.root(r"C:\Users\Alex");
    let docs = fx.dir(home, "Documents", 3);
    let reports = fx.dir(docs, "Reports", 2);
    let invoices = fx.dir(docs, "Invoices", 9);
    let taxes = fx.dir(docs, "Taxes", 120);
    let downloads = fx.dir(home, "Downloads", 1);
    let pictures = fx.dir(home, "Pictures", 4);
    let music = fx.dir(home, "Music", 30);
    let desktop = fx.dir(home, "Desktop", 0);
    let projects = fx.root(r"D:\Projects");
    let archive = fx.root(r"D:\Archive");

    // Reports: the common query.
    for (year, quarters) in [(2022, 4), (2023, 4), (2024, 4), (2025, 4), (2026, 3)] {
        for q in 1..=quarters {
            let age = 30 + (2026 - year) * 365 + (4 - q) * 90;
            fx.file(
                reports,
                &format!("Quarterly report {year}-Q{q}.pdf"),
                380 + 40 * q,
                age,
            );
            fx.file(
                reports,
                &format!("Sales report {year}-Q{q}.xlsx"),
                120 + 15 * q,
                age + 2,
            );
        }
        fx.file(
            reports,
            &format!("Annual report {year}.docx"),
            2_400 + year,
            60 + (2026 - year) * 365,
        );
    }
    for region in ["North", "South", "East", "West", "Central", "Overseas"] {
        for n in 1..=4 {
            fx.file(
                reports,
                &format!("report_{}_final_v{n}.xlsx", region.to_lowercase()),
                90 + n * 7,
                5 + n * 11,
            );
        }
    }
    fx.file(reports, "Report template (blank).dotx", 48, 400);
    fx.file(reports, "レポート 2024.docx", 220, 77);
    fx.file(reports, "営業会議 議事録 2026-10.docx", 64, 12);
    fx.file(
        invoices,
        "Invoice 2026-10 Northwind report attachment.pdf",
        311,
        18,
    );
    fx.file(invoices, "請求書 2026-10.pdf", 205, 17);
    fx.file(invoices, "Invoice 2026-09.pdf", 198, 48);
    fx.file(invoices, "Invoice 2026-08.pdf", 202, 79);
    fx.file(taxes, "Tax return 2025.pdf", 1_850, 210);
    fx.file(taxes, "Tax report 2025 (accountant).xlsx", 540, 205);
    fx.file(downloads, "report-final-FINAL(2).pdf", 912, 1);
    fx.file(downloads, "ferrow-0.1.0-beta.16-setup.exe", 6_420, 2);
    fx.file(downloads, "Windows11_23H2_English_x64.iso", 6_100_000, 190);
    fx.file(desktop, "todo.txt", 1, 0);
    fx.file(desktop, "Budget 2027 draft.xlsx", 88, 0);
    fx.file(desktop, "Meeting notes - report review.md", 4, 0);

    // Pictures: a trip with Japanese folder names, plus filler.
    let trip = fx.dir(pictures, "東京旅行 2026", 40);
    let year = fx.dir(pictures, "2025", 300);
    for n in 0..320 {
        fx.file(
            trip,
            &format!("IMG_{:04}.jpg", 2000 + n),
            3_100 + n % 900,
            40,
        );
    }
    for n in 0..220 {
        fx.file(
            year,
            &format!("DSC_{:04}.jpg", 100 + n),
            4_800 + n % 700,
            300,
        );
    }
    fx.file(trip, "パノラマ 富士山.jpg", 14_500, 39);
    fx.file(pictures, "Screenshot 2026-11-14 091522.png", 612, 3);

    // Music.
    for (artist, albums) in [("Daft Punk", 3), ("宇多田ヒカル", 2), ("Khruangbin", 3)] {
        let a = fx.dir(music, artist, 200);
        for al in 1..=albums {
            let album = fx.dir(a, &format!("Album {al}"), 200 + al * 30);
            for t in 1..=11 {
                fx.file(album, &format!("{t:02} Track {t}.mp3"), 4_200 + t * 13, 200);
            }
        }
    }

    // Projects: deep paths and code.
    let ferrow = fx.dir(projects, "ferrow", 0);
    let crates = fx.dir(ferrow, "crates", 0);
    for name in ["ferrow-gui", "ferrow-index", "ferrow-query", "ferrowd"] {
        let c = fx.dir(crates, name, 1);
        let src = fx.dir(c, "src", 1);
        for f in ["lib.rs", "main.rs", "engine.rs", "settings.rs", "report.rs"] {
            let (kb, age) = (6 + fx.next() % 30, 1 + fx.next() % 20);
            fx.file(src, f, kb, age);
        }
        fx.file(c, "Cargo.toml", 1, 8);
    }
    fx.file(ferrow, "README.md", 6, 2);
    fx.file(ferrow, "CHANGELOG.md", 14, 2);
    let mut deep = fx.dir(projects, "client-portal", 50);
    for part in [
        "packages",
        "reporting-service",
        "src",
        "internal",
        "platform",
        "windows",
        "integration",
        "shell",
        "extensions",
        "handlers",
    ] {
        deep = fx.dir(deep, part, 50);
    }
    fx.file(
        deep,
        "report-generator-with-a-very-long-descriptive-file-name.ts",
        18,
        50,
    );
    fx.file(deep, "report.test.ts", 4, 50);
    let nm = fx.dir(projects, "node_modules", 50);
    for n in 0..260 {
        let pkg = fx.dir(nm, &format!("left-pad-{n}"), 50);
        fx.file(pkg, "index.js", 3, 50);
        fx.file(pkg, "package.json", 1, 50);
    }

    // Archive: a long customer path.
    let mut cur = archive;
    for part in [
        "Clients",
        "Northwind Traders",
        "2023",
        "Quarterly Reviews",
        "Q4 Board Pack",
        "Supporting Documents",
    ] {
        cur = fx.dir(cur, part, 700);
    }
    fx.file(
        cur,
        "Annual report - draft for review (final, signed, scanned).pdf",
        18_400,
        690,
    );
    fx.file(cur, "report appendix A.pdf", 3_100, 690);
    for n in 0..120 {
        fx.file(
            archive,
            &format!("backup-2021-{n:03}.zip"),
            52_000 + n * 11,
            900,
        );
    }

    Arc::new(fx.b.freeze())
}

/// Rows as the Ferrow service would send them (for the "service" screen).
fn service_rows() -> Vec<ferrow_ipc::FileRow> {
    let row = |path: &str, kb: u64, days: i64, is_dir: bool| ferrow_ipc::FileRow {
        path: path.as_bytes().to_vec(),
        size: if is_dir { None } else { Some(kb * 1024) },
        modified: Some((NOW - days * 86_400) * 10_000_000),
        is_dir,
        ..Default::default()
    };
    let mut rows = vec![
        row(r"C:\Users\Alex\Documents\Reports", 0, 2, true),
        row(
            r"C:\Users\Alex\Documents\Reports\Annual report 2025.docx",
            2_425,
            400,
            false,
        ),
        row(
            r"C:\Users\Alex\Documents\Reports\Quarterly report 2026-Q3.pdf",
            500,
            30,
            false,
        ),
        row(
            r"D:\Archive\Clients\Northwind Traders\2023\Quarterly Reviews\Q4 Board Pack\Supporting Documents\Annual report - draft for review (final, signed, scanned).pdf",
            18_400,
            690,
            false,
        ),
        row(
            r"D:\Projects\ferrow\crates\ferrow-gui\src\report.rs",
            12,
            3,
            false,
        ),
        row(
            r"C:\Windows\System32\drivers\etc\report-agent.log",
            3,
            1,
            false,
        ),
        row(
            r"C:\ProgramData\Microsoft\Windows\WER\ReportArchive",
            0,
            90,
            true,
        ),
        row(
            r"C:\Users\Alex\Downloads\report-final-FINAL(2).pdf",
            912,
            1,
            false,
        ),
    ];
    for n in 0..80 {
        rows.push(row(
            &format!(r"C:\ProgramData\Microsoft\Windows\WER\ReportArchive\Report{n:04}.wer"),
            2 + n % 9,
            90 + n as i64,
            false,
        ));
    }
    rows
}

fn fixture_settings(theme: ThemeChoice) -> Settings {
    let mut s = Settings::default();
    s.theme = theme;
    s.sources = vec![
        Source {
            kind: SourceKind::Folder,
            path: PathBuf::from(r"C:\Users\Alex"),
        },
        Source {
            kind: SourceKind::Drive,
            path: PathBuf::from(r"D:\"),
        },
        Source {
            kind: SourceKind::NetworkFolder,
            path: PathBuf::from(r"\\nas\media\photos"),
        },
    ];
    s.excludes = vec![
        PathBuf::from(r"C:\Users\Alex\AppData\Local\Temp"),
        PathBuf::from(r"D:\Projects\node_modules"),
    ];
    // Never scan from a screenshot run.
    s.auto_rebuild = AutoRebuild::Never;
    s
}

// -------------------------------------------------------------------
// Harness

/// The real app plus the window background the OS would show behind it.
struct Shot {
    app: FerrowApp,
    events: Sender<Event>,
}

impl eframe::App for Shot {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        self.app.clear_color(visuals)
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // The window colour (what eframe clears to before painting).
        let [r, g, b, a] = self.app.clear_color(ui.visuals());
        let to_u8 = |c: f32| (c * 255.0).round() as u8;
        let bg = Color32::from_rgba_unmultiplied(to_u8(r), to_u8(g), to_u8(b), to_u8(a));
        ui.painter().rect_filled(ui.max_rect(), 0.0, bg);
        self.app.ui(ui, frame);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Dark,
    Light,
}

impl Look {
    const BOTH: [Look; 2] = [Look::Dark, Look::Light];

    fn name(self) -> &'static str {
        match self {
            Look::Dark => "dark",
            Look::Light => "light",
        }
    }

    fn choice(self) -> ThemeChoice {
        match self {
            Look::Dark => ThemeChoice::Dark,
            Look::Light => ThemeChoice::Light,
        }
    }

    fn theme(self) -> egui::Theme {
        match self {
            Look::Dark => egui::Theme::Dark,
            Look::Light => egui::Theme::Light,
        }
    }
}

/// One app instance in one window size and theme.
struct Session<'a> {
    harness: Harness<'a, Shot>,
    index: Arc<ferrow_index::Index>,
    size: Vec2,
    look: Look,
    out: PathBuf,
}

impl<'a> Session<'a> {
    /// `with_index`: whether the isolated worker searches the fixture index
    /// (the "service" screen uses no local index at all).
    fn new(
        look: Look,
        size: Vec2,
        out: &Path,
        index: &Arc<ferrow_index::Index>,
        with_index: bool,
    ) -> Self {
        let index = index.clone();
        let worker_index = with_index.then(|| index.clone());
        let harness = Harness::builder()
            .with_size(size)
            .with_pixels_per_point(1.0)
            .with_theme(look.theme())
            .allow_missing_glyphs()
            .with_render_cursor(false)
            .wgpu()
            .build_eframe(move |cc| {
                let ctx = cc.egui_ctx.clone();
                theme::install_fonts(&ctx).expect("a UI font is installed on this machine");
                let settings = fixture_settings(look.choice());
                let (dark, palette) = FerrowApp::apply_look(&ctx, &settings, None);
                let (engine, events) = Engine::isolated(worker_index);
                let mut app =
                    FerrowApp::assemble(engine, settings, palette, false, dark, None, false);
                // No update check: it would call the network.
                app.updater.opened = true;
                Shot { app, events }
            });
        Session {
            harness,
            index,
            size,
            look,
            out: out.to_path_buf(),
        }
    }

    fn app(&mut self) -> &mut FerrowApp {
        &mut self.harness.state_mut().app
    }

    fn send(&mut self, event: Event) {
        self.harness
            .state()
            .events
            .send(event)
            .expect("the isolated engine is running");
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.harness.step();
        }
    }

    /// Step until `done` holds (results arrive from the worker thread).
    fn wait(&mut self, what: &str, done: impl Fn(&FerrowApp) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.harness.step();
            if done(&self.harness.state().app) {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
        self.frames(2);
    }

    /// The index finished loading (what the engine reports at start).
    fn loaded(&mut self) {
        let index = self.index.clone();
        self.send(Event::Loaded {
            index,
            elapsed: Duration::from_millis(41),
            source: r"C:\Users\Alex\AppData\Local\ferrow\index.bin".into(),
        });
        self.wait("results of the empty query", |a| a.hits.is_some());
        self.fix_timing();
    }

    /// The Ferrow service answers and the list shows its results for
    /// "report" (live, all drives).
    fn service_results(&mut self) {
        self.send(Event::Remote {
            entries: 18_422_907,
            report: "NTFS journal, 3 volumes".into(),
        });
        self.frames(3);
        self.app().query = "report".into();
        self.frames(2);
        self.send(Event::Results {
            generation: 1,
            hits: engine::service_hits_for_test(1, 2_413, service_rows()),
            complete: true,
            elapsed: Duration::from_micros(900),
            sort_pending: false,
            sort_unavailable: false,
        });
        self.frames(3);
    }

    /// Search times are the only non-deterministic text; pin them.
    fn fix_timing(&mut self) {
        let app = self.app();
        app.search_time = Duration::from_micros(300);
        app.load_time = Duration::from_millis(41);
        self.frames(1);
    }

    fn query(&mut self, text: &str) {
        let before = self.app().shown_generation;
        self.app().query = text.to_owned();
        self.wait("results of the query", move |a| {
            a.shown_generation > before
                && (a.query_error.is_some() || (a.hits.is_some() && a.complete && !a.sort_pending))
        });
        self.fix_timing();
    }

    fn sort_by(&mut self, sort: SortChoice, descending: bool) {
        let app = self.app();
        app.settings.sort = sort;
        app.settings.sort_descending = descending;
        let before = app.shown_generation;
        app.sent_query = None;
        self.wait("results in the new sort order", move |a| {
            a.shown_generation > before && a.hits.is_some() && a.complete && !a.sort_pending
        });
        self.fix_timing();
    }

    /// Where the content area starts (below the title row on Windows).
    fn content_top(&self) -> f32 {
        if platform::CUSTOM_TITLE_ROW {
            TITLE_HEIGHT
        } else {
            56.0
        }
    }

    /// Centre of result row `row` (0 = first visible).
    fn row_center(&self, row: usize) -> Pos2 {
        let list_top = self.content_top() + 1.0 + HEADER_HEIGHT;
        pos2(300.0, list_top + ROW_HEIGHT * row as f32 + ROW_HEIGHT / 2.0)
    }

    fn hover(&mut self, pos: Pos2) {
        self.harness.hover_at(pos);
        self.frames(2);
    }

    /// Right-click on the column header: the column chooser.
    fn open_column_menu(&mut self) {
        let pos = pos2(150.0, self.content_top() + 1.0 + HEADER_HEIGHT / 2.0);
        self.hover(pos);
        for pressed in [true, false] {
            self.harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        self.frames(3);
    }

    /// Click the state chip at the left of the status bar.
    fn click_state_chip(&mut self) {
        let pos = pos2(60.0, self.size.y - 16.0);
        self.hover(pos);
        for pressed in [true, false] {
            self.harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        self.frames(8);
    }

    /// Click the "?" next to the search box: the search syntax help.
    fn open_syntax_help(&mut self) {
        let w = self.size.x;
        let (x, y) = if platform::CUSTOM_TITLE_ROW {
            let left = 110.0;
            let right = w - CAPTION_BUTTONS_WIDTH - GEAR_WIDTH - 8.0;
            let width = (right - left).clamp(120.0, 640.0);
            let x = (w / 2.0 - width / 2.0).max(left).min(right - width);
            (x + width, TITLE_HEIGHT / 2.0)
        } else {
            (w - 12.0, 28.0)
        };
        let query_empty = self.app().query.is_empty();
        let clear_w = if query_empty { 0.0 } else { 32.0 };
        let pos = pos2(x - 12.0 - clear_w - 15.0, y);
        self.hover(pos);
        for pressed in [true, false] {
            self.harness.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        self.frames(3);
    }

    /// Show the Settings page (the gear button sets exactly this).
    fn settings_page(&mut self) {
        self.app().page = Page::Settings;
        self.app().focus_search = false;
        self.frames(3);
    }

    /// Scroll Settings to a section the way a request from another page
    /// (or a click in the section list) does.
    fn goto(&mut self, target: &'static str) {
        self.app().settings_scroll_to = Some(target);
        self.frames(4);
    }

    /// Wheel scroll by `dy` points (negative: towards the bottom).
    fn scroll_by(&mut self, dy: f32) {
        let at = pos2(self.size.x / 2.0, self.size.y / 2.0);
        self.hover(at);
        self.wheel(at, dy);
        // egui spreads a wheel step over several frames; let it finish so it
        // cannot leak into the next picture.
        self.frames(40);
    }

    /// Back to the top of the page (a big wheel step up).
    fn scroll_top(&mut self) {
        self.scroll_by(100_000.0);
    }

    /// Positive `dy` moves the content down (scrolls towards the top).
    fn wheel(&mut self, at: Pos2, dy: f32) {
        self.harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, dy),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
        });
        let _ = at;
        self.frames(3);
    }

    fn save(&mut self, number: u32, name: &str) {
        let file = self.out.join(format!(
            "{number:02}-{name}-{}-{}x{}.png",
            self.look.name(),
            self.size.x as u32,
            self.size.y as u32
        ));
        let image = self
            .harness
            .render()
            .unwrap_or_else(|e| panic!("could not render {}: {e}", file.display()));
        // A frame with nothing drawn would be a useless picture.
        assert!(
            image.pixels().any(|p| p.0[3] != 0),
            "{} is empty",
            file.display()
        );
        write_png(&file, image.width(), image.height(), image.as_raw())
            .unwrap_or_else(|e| panic!("could not write {}: {e}", file.display()));
        println!("wrote {}", file.display());
    }
}

// -------------------------------------------------------------------
// The run

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())
}

fn output_dir() -> PathBuf {
    let dir = match std::env::var_os("FERROW_SCREENSHOT_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => std::env::temp_dir().join("ferrow-screenshots"),
    };
    std::fs::create_dir_all(&dir).expect("create the screenshot folder");
    let dir = std::fs::canonicalize(&dir).expect("resolve the screenshot folder");
    // `canonicalize` gives `\\?\C:\...` on Windows; the app shows this path
    // in Settings, so use the plain form.
    let text = dir.to_string_lossy().into_owned();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

#[test]
#[ignore = "renders screenshots; run explicitly (see the module docs)"]
fn render_all_screens() {
    let out = output_dir();
    println!("screenshots go to {}", out.display());
    // The only data location the app can see in this process.
    let data = out.join(".test-data");
    std::fs::create_dir_all(&data).expect("create the scratch data folder");
    engine::test_data_dir::set(&data);
    let result = std::panic::catch_unwind(|| capture_all(&out));
    engine::test_data_dir::clear();
    let _ = std::fs::remove_dir_all(&data);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Settings scrolled so a section fills most of the view, and to the start
/// of Updates: the highlight in the section list and the top edge of the
/// scroll area (FR-35). Also at the owner's window size. Pictures are named
/// `4N-settings-*-<theme>-WxH.png`.
#[test]
#[ignore = "renders screenshots; run explicitly (see the module docs)"]
fn render_settings_scroll_edge() {
    let out = output_dir();
    let data = out.join(".test-data");
    std::fs::create_dir_all(&data).expect("create the scratch data folder");
    engine::test_data_dir::set(&data);
    let result = std::panic::catch_unwind(|| {
        let index = fixture_index();
        for look in Look::BOTH {
            for size in [WIDE, OWNER] {
                let mut s = Session::new(look, size, &out, &index, false);
                s.settings_page();
                // The Windows heading a third of the way down, the tail of
                // the own index above it (the owner's picture).
                s.goto("Windows");
                s.scroll_by((size.y - s.content_top() - 52.0) * 0.36);
                s.save(41, "settings-edge-windows");
                // The start of Updates, a little of Appearance above it.
                s.goto("Updates");
                s.scroll_by(10.0);
                s.save(42, "settings-edge-updates");
                // The very end.
                s.scroll_by(-100_000.0);
                s.save(43, "settings-edge-end");
                // A click in the section list on a short section: it lands
                // at the top and is the one highlighted.
                s.goto("Appearance");
                s.save(44, "settings-click-appearance");
                s.goto("Updates");
                s.save(45, "settings-click-updates");
            }
        }
    });
    engine::test_data_dir::clear();
    let _ = std::fs::remove_dir_all(&data);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Whether `name` is a picture this harness writes
/// (`NN-screen-dark|light-WxH.png`). Only those are removed before a run, so
/// pointing `FERROW_SCREENSHOT_DIR` at a folder with other pictures is safe.
fn is_harness_picture(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".png") else {
        return false;
    };
    let b = stem.as_bytes();
    if b.len() < 4 || !b[0].is_ascii_digit() || !b[1].is_ascii_digit() || b[2] != b'-' {
        return false;
    }
    let Some((rest, size)) = stem.rsplit_once('-') else {
        return false;
    };
    let size_ok = size.split_once('x').is_some_and(|(w, h)| {
        !w.is_empty()
            && !h.is_empty()
            && w.bytes().all(|c| c.is_ascii_digit())
            && h.bytes().all(|c| c.is_ascii_digit())
    });
    size_ok && (rest.ends_with("-dark") || rest.ends_with("-light"))
}

fn capture_all(out: &Path) {
    // Stale files from an older run would be mistaken for fresh ones.
    for entry in std::fs::read_dir(out).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_harness_picture)
        {
            let _ = std::fs::remove_file(p);
        }
    }
    let index = fixture_index();

    for look in Look::BOTH {
        for size in [WIDE, NARROW] {
            let new = |with_index| Session::new(look, size, out, &index, with_index);

            // 01 First start: no index yet (what a new user sees).
            let mut s = new(true);
            s.frames(2);
            s.save(1, "loading");
            s.send(Event::NoIndex);
            s.frames(3);
            s.save(2, "first-start");

            // 03 Indexing in progress (no search yet), with the engine's
            // progress report.
            s.send(Event::Scanning(
                r"Indexing C:\Users\Alex, D:\, \\nas\media\photos…".into(),
            ));
            s.send(Event::ScanProgress {
                entries: 1_204_331,
                location: r"D:\Projects\client-portal\packages\reporting-service\src\internal\platform\windows".into(),
                elapsed: Duration::from_secs(12),
            });
            s.frames(3);
            s.save(3, "indexing");

            // 04 Index ready, empty search box: everything, in name order.
            let mut s = new(true);
            s.loaded();
            s.save(4, "empty-search");

            // 05 Results for a common query, sorted by name.
            s.query("report");
            s.save(5, "results-name");

            // 06 The same results by date modified, newest first.
            s.sort_by(SortChoice::Modified, true);
            s.save(6, "results-date-desc");

            // 07 No results.
            let mut s = new(true);
            s.loaded();
            s.query("zzqx quarterly");
            s.save(7, "no-results");

            // 08 A query that does not parse (unbalanced group) while typing:
            // the last results stay, dimmed, and the message is inline.
            let mut s = new(true);
            s.loaded();
            s.query("report");
            s.query("<report | invoice");
            s.save(8, "query-error");

            // 09 The service stopped answering: the results stay, a notice
            // bar explains, the app continues with its own index.
            let mut s = new(false);
            s.service_results();
            s.send(Event::ServiceLost {
                error: "The pipe has been ended. (os error 109)".into(),
            });
            s.frames(3);
            s.save(9, "service-lost");

            // 10 Results from the Ferrow service (live, all drives).
            let mut s = new(false);
            s.service_results();
            s.save(10, "service-results");

            // 24 The service refuses this account: own index, with the
            // reason in the chip.
            let mut s = new(true);
            s.send(Event::ServiceGap(engine::ServiceGap::Refused));
            s.loaded();
            s.query("report");
            s.save(24, "service-refused");

            // 25 No results for a search with filters.
            let mut s = new(true);
            s.loaded();
            s.query("zzqx quarterly ext:pdf dm:today");
            s.save(25, "no-results-filters");

            // 26 Rebuilding while the current index stays searchable:
            // progress only in the status chip.
            let mut s = new(true);
            s.loaded();
            s.query("report");
            s.send(Event::Scanning(
                r"Indexing C:\Users\Alex, D:\, \\nas\media\photos…".into(),
            ));
            s.send(Event::ScanProgress {
                entries: 1_204_331,
                location: r"D:\Projects\ferrow\crates".into(),
                elapsed: Duration::from_secs(12),
            });
            s.frames(3);
            s.save(26, "indexing-rebuild");

            // 27 First start when the service refuses this account.
            let mut s = new(true);
            s.send(Event::ServiceGap(engine::ServiceGap::Refused));
            s.send(Event::NoIndex);
            s.frames(3);
            s.save(27, "first-start-refused");

            // 28 Japanese text in the Folder and Name columns sits on the
            // same baseline as the Latin text.
            let mut s = new(true);
            s.loaded();
            s.query("Track 1");
            s.save(28, "japanese-rows");

            // 30 A service-only PC while the service restarts: the engine
            // reports the loss and that there is no index of its own. The
            // rows and the notice stay; this is not the first-start page.
            let mut s = new(false);
            s.service_results();
            s.send(Event::ServiceLost {
                error: "The pipe has been ended. (os error 109)".into(),
            });
            s.send(Event::NoIndex);
            s.frames(3);
            s.save(30, "service-lost-no-own-index");

            // 29 Long names keep their extension; long paths keep the drive
            // and the last folders.
            let mut s = new(true);
            s.loaded();
            s.query("Annual report");
            s.save(29, "long-names-and-paths");
        }
    }

    // Menus and panels: both sizes (the syntax panel must fit the narrow
    // window).
    for look in Look::BOTH {
        for size in [WIDE, NARROW] {
            let new = |with_index| Session::new(look, size, out, &index, with_index);

            // 14 Column chooser (right-click on the header).
            let mut s = new(true);
            s.loaded();
            s.query("report");
            s.open_column_menu();
            s.save(14, "column-menu");

            // 15 Search syntax help.
            let mut s = new(true);
            s.loaded();
            s.open_syntax_help();
            s.save(15, "syntax-help");
        }
    }

    // Settings, at both sizes (the section list only stands at the wide one).
    for look in Look::BOTH {
        for size in [WIDE, NARROW] {
            settings_scenes(look, size, out, &index);
        }
    }

    // States that only make sense at the default size.
    for look in Look::BOTH {
        let new = |with_index| Session::new(look, WIDE, out, &index, with_index);

        // 11 Selected row, 12 hovered row, 13 both.
        let mut s = new(true);
        s.loaded();
        s.query("report");
        s.app().selected = Some(3);
        s.frames(2);
        s.save(11, "row-selected");
        let hover = s.row_center(1);
        s.hover(hover);
        s.save(12, "row-selected-and-hover");
        let mut s = new(true);
        s.loaded();
        s.query("report");
        let hover = s.row_center(2);
        s.hover(hover);
        s.save(13, "row-hover");

        // 31 Clicking the state chip opens Settings at the Fast index
        // section (checked here end to end: the click sets the target, the
        // Settings page takes it and scrolls).
        let mut s = new(true);
        s.loaded();
        s.query("report");
        s.click_state_chip();
        assert!(s.app().page == Page::Settings, "the chip opens Settings");
        assert!(
            s.app().settings_scroll_to.is_none(),
            "the Settings page took the scroll target"
        );
        s.save(31, "chip-opens-service-access");
    }
}

/// Settings (16..39): the fast-index status in every state, who can use the
/// service, the own-index group closed and open, and each section.
fn settings_scenes(look: Look, size: Vec2, out: &Path, index: &Arc<ferrow_index::Index>) {
    use ferrow_ipc::{AccessInfo, AccessMode, AccessUser};
    let user = |id: &str, name: &str| AccessUser {
        id: id.into(),
        name: name.into(),
    };
    let info = |mode, allowed, users: Option<Vec<AccessUser>>| {
        access::View::Info(AccessInfo {
            mode,
            you_allowed: allowed,
            you: user(
                "S-1-5-21-3623811015-3361044348-30300820-1013",
                r"ALEX-PC\Alex",
            ),
            users,
        })
    };
    access::test_can_elevate(true);
    settings_page::test_set_now(NOW as u64);

    // The service is not available: the app's own index, one reason each.
    let mut s = Session::new(look, size, out, index, true);
    s.loaded();
    s.settings_page();
    s.save(16, "settings-off-no-service");
    s.goto("Own index");
    s.save(17, "settings-own-index-open");
    s.app()
        .access
        .show_for_test(access::View::TooOld, false, None);
    s.goto("Fast index");
    s.save(18, "settings-off-old-service");
    let refused = info(AccessMode::Admins, false, None);
    s.app().access.show_for_test(refused.clone(), false, None);
    s.frames(2);
    s.save(19, "settings-off-refused");
    access::test_can_elevate(false);
    s.frames(2);
    s.save(20, "settings-off-refused-terminal");
    access::test_can_elevate(true);
    let declined = access::Notice::new(
        access::Outcome::Declined,
        "Administrator approval was declined; nothing was changed.",
    );
    s.app().access.show_for_test(refused, false, Some(declined));
    s.frames(2);
    s.save(21, "settings-access-declined");

    // The service answers: on, and who can use it.
    let mut s = Session::new(look, size, out, index, false);
    s.send(Event::Remote {
        entries: 18_422_907,
        report: "NTFS journal, 3 volumes".into(),
    });
    s.frames(3);
    s.settings_page();
    let admins = info(AccessMode::Admins, true, Some(Vec::new()));
    s.app().access.show_for_test(admins.clone(), false, None);
    s.frames(2);
    s.save(22, "settings-live-admins");
    let listed = info(
        AccessMode::Listed,
        true,
        Some(vec![
            user(
                "S-1-5-21-3623811015-3361044348-30300820-1014",
                r"ALEX-PC\Sam",
            ),
            user(
                "S-1-5-21-3623811015-3361044348-30300820-1015",
                r"ALEX-PC\Kim",
            ),
        ]),
    );
    s.app().access.show_for_test(listed, false, None);
    s.frames(2);
    s.save(23, "settings-live-listed");
    let everyone = info(AccessMode::Everyone, true, Some(Vec::new()));
    s.app().access.show_for_test(everyone.clone(), false, None);
    s.frames(2);
    s.save(24, "settings-live-everyone");
    s.app().access.show_for_test(admins.clone(), true, None);
    s.frames(2);
    s.save(25, "settings-access-busy");
    s.app().access.show_for_test(admins.clone(), false, None);
    s.app().access.confirm_everyone = true;
    s.frames(2);
    s.save(26, "settings-access-confirm");
    s.app().access.confirm_everyone = false;
    let done = access::Notice::new(
        access::Outcome::Done,
        "Done. The service applies it within a second.",
    );
    s.app().access.show_for_test(everyone, false, Some(done));
    s.frames(2);
    s.save(27, "settings-access-done");
    s.app().access.show_for_test(admins.clone(), false, None);
    s.frames(2);

    // The "Add account" field: empty, looked up, and an unknown name.
    let add = |s: &mut Session, text: &str, state: access::AddState, number, name| {
        s.app().access.show_for_test(admins.clone(), false, None);
        s.app().access.add = Some(access::AddAccount {
            text: text.into(),
            state,
            focus: false,
        });
        s.goto("Fast index");
        if size == NARROW {
            s.scroll_top();
            s.scroll_by(-230.0);
        }
        s.save(number, name);
    };
    add(
        &mut s,
        "",
        access::AddState::Idle,
        35,
        "settings-add-account-empty",
    );
    let sam = access::Resolved {
        id: "S-1-5-21-3623811015-3361044348-30300820-1014".into(),
        name: r"ALEX-PC\Sam".into(),
    };
    add(
        &mut s,
        "sam",
        access::AddState::Found(sam),
        36,
        "settings-add-account-found",
    );
    add(
        &mut s,
        "samm",
        access::AddState::Failed(
            "No account named \"samm\" was found on this PC or its domain.".into(),
        ),
        37,
        "settings-add-account-unknown",
    );
    // Without the administrator button the command is shown: with a real
    // domain SID it is long, so it wraps at the narrow size.
    access::test_can_elevate(false);
    let sam = access::Resolved {
        id: "S-1-5-21-3623811015-3361044348-30300820-1014".into(),
        name: r"ALEX-PC\Sam".into(),
    };
    add(
        &mut s,
        "sam",
        access::AddState::Found(sam),
        38,
        "settings-add-account-command",
    );
    access::test_can_elevate(true);
    // The change ran but this account cannot see the list.
    s.app().access.add = None;
    let unconfirmed = access::Notice::new(
        access::Outcome::Unconfirmed,
        "Added — the list isn't visible to this account, so Ferrow can't confirm it.",
    );
    s.app().access.show_for_test(
        info(AccessMode::Listed, true, None),
        false,
        Some(unconfirmed),
    );
    s.goto("Fast index");
    if size == NARROW {
        s.scroll_top();
        s.scroll_by(-120.0);
    }
    s.save(39, "settings-access-unconfirmed");
    s.app().access.show_for_test(admins.clone(), false, None);
    s.frames(2);

    // The own-index group: closed while the service is live, then opened.
    s.goto("Fast index");
    s.scroll_top();
    s.scroll_by(-240.0);
    s.save(28, "settings-live-own-index-closed");
    s.goto("Own index");
    s.save(29, "settings-live-own-index-open");

    // The other sections.
    s.goto("Windows");
    s.save(30, "settings-windows");
    s.goto("Appearance");
    s.save(31, "settings-appearance");
    s.app().settings.last_update_check = Some(NOW as u64 - 3 * 3600);
    s.app().updater.state = UpdateState::UpToDate;
    s.goto("Updates");
    s.save(32, "settings-updates");
    s.app().updater.state = UpdateState::Available(ferrow_update::Release {
        version: ferrow_update::Version {
            major: 0,
            minor: 1,
            patch: 0,
            pre: vec!["beta".into(), "17".into()],
        },
        has_setup: true,
    });
    crate::update::test_can_install(true);
    s.goto("Updates");
    s.save(33, "settings-update-available");
    crate::update::test_can_install(false);
    s.goto("About");
    s.save(34, "settings-about");
}

/// A scroll request made on the very first frame of a freshly opened
/// Settings page reaches its section (the room below the last section is not
/// measured yet on that frame).
#[test]
#[ignore = "needs the wgpu adapter like the screenshot run"]
fn a_scroll_request_on_the_first_frame_reaches_its_section() {
    let index = fixture_index();
    let out = std::env::temp_dir();
    let mut sections = vec!["Own index", "Appearance", "Updates", "About"];
    if cfg!(windows) {
        sections.push("Windows");
    }
    sections.push("Fast index");
    for size in [WIDE, OWNER] {
        for target in &sections {
            let mut s = Session::new(Look::Dark, size, &out, &index, false);
            s.app().page = Page::Settings;
            s.app().focus_search = false;
            s.app().settings_scroll_to = Some(target);
            s.frames(6);
            let seen = settings_page::section_label_in_view(&s.harness.ctx);
            assert_eq!(seen, *target, "request on the first frame at {size:?}");
        }
    }
}

#[cfg(test)]
mod picture_names {
    use super::is_harness_picture;

    #[test]
    fn only_the_harness_pictures_are_cleaned_up() {
        assert!(is_harness_picture("05-results-name-dark-1040x680.png"));
        assert!(is_harness_picture(
            "23-settings-service-index-light-560x360.png"
        ));
        for other in [
            "keep.png",
            "Screenshot 2026-11-14 091522.png",
            "05-results-name-dark-1040x680.jpg",
            "05-results-name-sepia-1040x680.png",
            "5-results-dark-1040x680.png",
            "05-results-dark-1040.png",
            "holiday-dark-1x1.png",
        ] {
            assert!(!is_harness_picture(other), "{other} must be kept");
        }
    }
}
