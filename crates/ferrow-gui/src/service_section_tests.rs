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
    assert_eq!(row(&ok, "Items").unwrap().1, "1,234,567");
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
    assert!(detail.ends_with("(38.0 s, startup)"));

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
    assert!(detail.ends_with("rescan pending (journal reset)"));

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
