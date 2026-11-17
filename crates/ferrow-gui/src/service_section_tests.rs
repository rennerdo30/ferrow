use super::*;
use ferrow_ipc::health::SaveInfo;

fn row<'a>(
    rows: &'a [(&'static str, String, Option<Level>)],
    label: &str,
) -> Option<&'a (&'static str, String, Option<Level>)> {
    rows.iter().find(|r| r.0 == label)
}

fn report() -> Health {
    Health {
        version: APP_VERSION.into(),
        pid: 1,
        started_ms: 1_000,
        uptime_s: 90_000,
        working_set: 300 << 20,
        private_bytes: Some(200 << 20),
        entries: 1_234_567,
        access: "listed".into(),
        last_save: Some(SaveInfo {
            at_ms: 5_000,
            ok: true,
            error: String::new(),
            took_ms: 2_300,
        }),
        ..Health::default()
    }
}

fn scm(start_type: Option<u32>) -> service::Scm {
    service::Scm {
        state: service::ScmState::Running,
        pid: Some(1),
        start_type,
    }
}

#[test]
fn the_details_say_what_is_wrong_in_the_row_that_is_wrong() {
    let ok = details(&report(), &scm(Some(2)));
    assert_eq!(row(&ok, "Service version").unwrap().1, APP_VERSION);
    assert!(ok.iter().all(|r| r.2.is_none()), "nothing to warn about");
    // The headline's precision: a few more items must not change the text.
    assert_eq!(row(&ok, "Items").unwrap().1, "1.2 M");
    assert_eq!(
        row(&ok, "Memory").unwrap().1,
        "300 MB in use, 200 MB private"
    );
    assert_eq!(
        row(&ok, "Who can use it").unwrap().1,
        "administrators and the listed accounts"
    );
    assert_eq!(row(&ok, "Starts").unwrap().1, "automatically at boot");
    assert!(row(&ok, "Connection errors").is_none());

    // Live changes merged into the index in memory: said once, in the
    // service's own rows (not a scan, and not repeated on every volume).
    assert!(row(&ok, "Live changes").is_none());
    let mut merged = report();
    merged.live_stats.last_compaction = Some(health::ScanInfo {
        at_ms: 6_000,
        reason: "50001 live changes".into(),
        took_ms: 12_400,
    });
    let rows = details(&merged, &scm(Some(2)));
    let live_changes = row(&rows, "Live changes").unwrap();
    assert!(
        live_changes.1.starts_with("merged in memory ")
            && live_changes.1.ends_with(", took 12.4 s"),
        "{live_changes:?}"
    );
    assert_eq!(live_changes.2, None, "nothing to warn about");
    for v in &merged.volumes {
        assert!(!volume_lines(v).3.contains("merged"), "not on a volume row");
    }

    let mut old = report();
    old.version = "0.1.0-beta.1".into();
    let skew = details(&old, &scm(None));
    let version = row(&skew, "Service version").unwrap();
    assert_eq!(version.2, Some(Level::Caution));
    assert!(version.1.contains("0.1.0-beta.1") && version.1.contains(APP_VERSION));
    assert!(
        row(&skew, "Starts").is_none(),
        "no start type, no row (Linux, macOS)"
    );

    let mut bad = report();
    bad.last_save = Some(SaveInfo {
        at_ms: 5_000,
        ok: false,
        error: "disk full".into(),
        took_ms: 1,
    });
    bad.connection_errors = 12;
    let rows = details(&bad, &scm(Some(4)));
    let save = row(&rows, "Last save").unwrap();
    assert_eq!(save.2, Some(Level::Caution));
    assert!(save.1.contains("failed: disk full"));
    assert!(row(&rows, "Connection errors").unwrap().1.contains("12"));
    // A disabled service is called out.
    assert_eq!(row(&rows, "Starts").unwrap().2, Some(Level::Caution));

    let mut fresh = report();
    fresh.uptime_s = 20;
    fresh.last_save = None;
    let rows = details(&fresh, &scm(Some(3)));
    assert_eq!(row(&rows, "Running for").unwrap().1, "just started");
    assert_eq!(row(&rows, "Last save").unwrap().1, "not saved yet");
}

#[test]
fn a_volume_shows_its_mode_its_reason_and_what_it_knows() {
    use health::*;
    let live = VolumeHealth {
        root: "C:\\".into(),
        mode: MODE_LIVE.into(),
        entries: Some(11_204_551),
        journal: JOURNAL_OK.into(),
        last_scan: Some(ScanInfo {
            at_ms: 5_000,
            reason: "startup".into(),
            took_ms: 38_000,
        }),
        ..VolumeHealth::default()
    };
    let (root, level, mode, detail) = volume_lines(&live);
    assert_eq!(
        (root.as_str(), level, mode.as_str()),
        ("C:\\", Level::Good, "live")
    );
    assert!(detail.starts_with("11.2 M items · journal ok · last scan "));
    assert!(
        detail.ends_with(", took 38.0 s, when the service started"),
        "{detail}"
    );

    let lost = VolumeHealth {
        mode: MODE_UNAVAILABLE.into(),
        reason: "access denied".into(),
        journal: JOURNAL_ERROR.into(),
        journal_detail: "reader stopped".into(),
        rescan_pending: Some(RescanInfo {
            reason: "journal reset".into(),
            earliest_ms: 1,
        }),
        ..live.clone()
    };
    let (_, level, mode, detail) = volume_lines(&lost);
    assert_eq!(level, Level::Caution);
    assert_eq!(mode, "live updates unavailable: access denied");
    assert!(detail.contains("journal reader error: reader stopped"));
    assert!(detail.contains("rescan pending (journal reset), not before "));

    let walked = VolumeHealth {
        root: "E:\\Archive".into(),
        mode: MODE_WALK.into(),
        journal: JOURNAL_NONE.into(),
        ..VolumeHealth::default()
    };
    let (_, level, mode, detail) = volume_lines(&walked);
    assert_eq!(
        (level, mode.as_str()),
        (Level::Neutral, "directory walk, not live")
    );
    assert_eq!(detail, "", "no journal, no scan, no count: nothing to add");
}

#[test]
fn buttons_wrap_to_rows_that_fit_and_none_is_lost() {
    let widths = [90.0, 100.0, 120.0, 100.0, 150.0];
    // Wide: all five on one row (560 + gaps fits 700).
    assert_eq!(button_rows(&widths, 700.0, 8.0), vec![vec![0, 1, 2, 3, 4]]);
    // Narrow: the fifth does not fit next to the others and goes below, whole.
    assert_eq!(
        button_rows(&widths, 500.0, 8.0),
        vec![vec![0, 1, 2, 3], vec![4]]
    );
    // Very narrow: every row holds at least one button.
    let rows = button_rows(&widths, 120.0, 8.0);
    assert_eq!(rows.iter().flatten().count(), 5);
    assert!(rows.iter().all(|r| !r.is_empty()));
    // A hidden Start leaves four.
    assert_eq!(
        button_rows(&widths[1..], 500.0, 8.0),
        vec![vec![0, 1, 2, 3]]
    );
}

#[test]
fn service_health_follows_fast_index_and_is_a_scroll_target() {
    let all: Vec<Section> = Section::ALL.into_iter().filter(|s| s.shown()).collect();
    assert_eq!(all[0], Section::FastIndex);
    assert_eq!(all[1], Section::ServiceHealth);
    assert_eq!(Section::ServiceHealth.label(), "Service health");
    assert_eq!(
        Section::from_target("Service health"),
        Some(Section::ServiceHealth)
    );
    assert_eq!(
        Section::from_target("service health"),
        Some(Section::ServiceHealth)
    );
}

#[test]
fn an_event_says_its_level_in_words() {
    let at = |level: &str| health::EventInfo {
        at_ms: 1,
        level: level.into(),
        text: "journal reset".into(),
    };
    assert_eq!(event_line(&at(health::LEVEL_ERROR)), "Error: journal reset");
    assert_eq!(
        event_line(&at(health::LEVEL_WARN)),
        "Warning: journal reset"
    );
    assert_eq!(event_line(&at(health::LEVEL_INFO)), "journal reset");
}

#[test]
fn the_fast_index_card_names_the_button_that_fixes_a_stopped_service() {
    use crate::service::ScmState;
    let off = |manager| {
        fast_index_status(None, &access::View::NoService, manager)
            .reason
            .unwrap()
    };
    let stopped = off(Some(ScmState::Stopped));
    assert!(stopped.contains("Start it under Service health"));
    assert!(
        !stopped.contains("Setup"),
        "no Setup advice when Start exists"
    );
    let missing = off(Some(ScmState::NotInstalled));
    assert!(missing.contains("isn't installed") && missing.contains("Setup"));
    assert!(!missing.contains("Start it"));
    // Nothing known yet: the general text.
    assert!(off(None).contains("isn't installed or isn't running"));
    assert!(off(Some(ScmState::Running)).contains("isn't installed or isn't running"));
}

#[test]
fn the_nav_dot_and_the_headline_agree() {
    use crate::service::{HealthView, Scm, ScmState};
    let stopped = Snapshot {
        scm: Scm {
            state: ScmState::Stopped,
            pid: None,
            start_type: None,
        },
        health: HealthView::NoAnswer,
    };
    let line = service::summary(Some(&stopped), APP_VERSION);
    // A ring in the caution colour; the same pair the nav row is given.
    assert_eq!((line.level, line.filled), (Level::Caution, false));
    assert_eq!(tone_of(line.level), Tone::Caution);
}

#[test]
fn the_reasons_for_off_buttons_are_said_once_each() {
    use crate::service::{Actions, CHANGING, NEEDS_INSTALLED};
    let all = |can| Actions {
        start: Can::Hidden,
        restart: can,
        rescan: can,
        repair: can,
    };
    // Starting or stopping: one line for the three buttons.
    assert_eq!(
        off_reasons(&all(Can::No(CHANGING)), false),
        ["Restart, Rescan now, Repair: It is changing state. Wait a moment."]
    );
    // Different reasons stay separate; an enabled button has none.
    let mixed = Actions {
        restart: Can::Yes,
        rescan: Can::No("A rescan is queued already (see the volumes)."),
        repair: Can::Yes,
        ..all(Can::Yes)
    };
    assert_eq!(
        off_reasons(&mixed, false),
        ["Rescan now: A rescan is queued already (see the volumes)."]
    );
    // Not installed: the restart line says it, the rescan line would repeat it.
    let missing = Actions {
        restart: Can::No("The service isn't installed."),
        rescan: Can::No("The service isn't running."),
        repair: Can::Yes,
        ..all(Can::Yes)
    };
    assert_eq!(
        off_reasons(&missing, true),
        ["Restart: The service isn't installed."]
    );
    // A copy that cannot elevate: the commands below say it.
    assert!(off_reasons(&all(Can::No(NEEDS_INSTALLED)), false).is_empty());
}

#[test]
fn the_scan_reason_reads_as_the_end_of_a_sentence() {
    assert_eq!(scan_reason_text("startup"), "when the service started");
    assert_eq!(
        scan_reason_text("50001 live changes"),
        "after 50,001 live changes"
    );
    assert_eq!(
        scan_reason_text("1234567 replaced entries"),
        "after 1,234,567 replaced entries"
    );
    assert_eq!(
        scan_reason_text("requested by an administrator"),
        "requested by an administrator"
    );
    assert_eq!(
        scan_reason_text("a folder moved into an indexed location (too many changes)"),
        "after a folder moved into an indexed location (too many changes)"
    );
    // Text that only looks like a count is not reworded.
    assert_eq!(
        scan_reason_text("many live changes"),
        "after many live changes"
    );
}

#[test]
fn a_volume_line_says_how_long_the_scan_took_and_why_it_ran() {
    use health::*;
    let v = VolumeHealth {
        root: "E:\\".into(),
        mode: MODE_LIVE.into(),
        journal: JOURNAL_OK.into(),
        last_scan: Some(ScanInfo {
            at_ms: 5_000,
            reason: "50001 live changes".into(),
            took_ms: 198_000,
        }),
        ..VolumeHealth::default()
    };
    let (_, _, _, detail) = volume_lines(&v);
    assert!(
        detail.ends_with(", took 3 min 18 s, after 50,001 live changes"),
        "{detail}"
    );
}

#[test]
fn a_live_volume_that_could_not_re_check_its_journal_shows_the_warning() {
    use health::*;
    let warning = "could not re-check the change journal after reading the MFT (x); files changed just before the scan may show their old state until the next scan";
    let v = VolumeHealth {
        root: "F:\\".into(),
        mode: MODE_LIVE.into(),
        journal: JOURNAL_OK.into(),
        journal_detail: warning.into(),
        ..VolumeHealth::default()
    };
    let (_, level, mode, detail) = volume_lines(&v);
    assert_eq!(level, Level::Caution);
    assert_eq!(mode, "live, with a warning");
    assert_eq!(detail, format!("journal ok, but {warning}"));
    // Without a warning the volume stays plainly live.
    let clean = VolumeHealth {
        journal_detail: String::new(),
        ..v
    };
    assert_eq!(volume_lines(&clean).1, Level::Good);
}
