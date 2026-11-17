use super::*;

struct Fake<'a> {
    admin: bool,
    unsandboxed: bool,
    id: &'a str,
}
impl Caller for Fake<'_> {
    fn admin(&self) -> bool {
        self.admin
    }
    fn unsandboxed(&self) -> bool {
        self.unsandboxed
    }
    fn id(&self) -> Option<String> {
        Some(self.id.into())
    }
}

const fn fake(admin: bool, unsandboxed: bool, id: &str) -> Fake<'_> {
    Fake {
        admin,
        unsandboxed,
        id,
    }
}

fn policy(mode: AccessMode, ids: &[&str]) -> Policy {
    Policy {
        mode,
        users: ids
            .iter()
            .map(|id| AccessUser {
                id: (*id).into(),
                name: format!("name-{id}"),
            })
            .collect(),
    }
}

/// Windows SIDs and Unix uids go through the same decision.
fn matrix(system: &str, admin: &str, listed: &str, unlisted: &str) {
    let callers = [
        ("system", fake(true, true, system)),
        ("admin", fake(true, true, admin)),
        ("standard listed", fake(false, true, listed)),
        ("standard unlisted", fake(false, true, unlisted)),
        ("sandboxed admin", fake(false, false, admin)),
        ("sandboxed listed", fake(false, false, listed)),
    ];
    // Expected answers in the modes: admins, everyone, listed.
    let expect = [
        ("system", [true, true, true]),
        ("admin", [true, true, true]),
        ("standard listed", [false, true, true]),
        ("standard unlisted", [false, true, false]),
        ("sandboxed admin", [false, false, false]),
        ("sandboxed listed", [false, false, false]),
    ];
    let modes = [AccessMode::Admins, AccessMode::Everyone, AccessMode::Listed];
    for ((name, caller), (expected_name, row)) in callers.iter().zip(expect) {
        assert_eq!(*name, expected_name);
        for (mode, want) in modes.into_iter().zip(row) {
            let p = policy(mode, &[listed]);
            assert_eq!(p.allows(caller), want, "{name} in {mode:?}");
        }
    }
}

#[test]
fn decision_matrix_windows_sids() {
    matrix(
        "S-1-5-18",
        "S-1-5-21-1-2-3-500",
        "S-1-5-21-1-2-3-1001",
        "S-1-5-21-1-2-3-1002",
    );
}

#[test]
fn decision_matrix_unix_uids() {
    matrix("0", "0", "1000", "1001");
}

#[test]
fn a_listed_standard_user_is_refused_by_the_administrators_only_rule() {
    // The rule before the policy was `admin()` alone: it refuses this caller.
    let listed = fake(false, true, "S-1-5-21-1-2-3-1001");
    assert!(!policy(AccessMode::Admins, &["S-1-5-21-1-2-3-1001"]).allows(&listed));
    assert!(policy(AccessMode::Listed, &["S-1-5-21-1-2-3-1001"]).allows(&listed));
    // Ids match case-insensitively, as SID text can differ in case.
    assert!(policy(AccessMode::Listed, &["s-1-5-21-1-2-3-1001"]).allows(&listed));
}

#[test]
fn a_caller_without_identity_is_refused_in_every_mode() {
    let unknown = CallerIdentity::default();
    for mode in [AccessMode::Admins, AccessMode::Everyone, AccessMode::Listed] {
        assert!(!policy(mode, &["0", "1000"]).allows(&unknown), "{mode:?}");
    }
}

#[test]
fn access_info_tells_a_refused_caller_only_mode_verdict_and_own_identity() {
    let p = policy(AccessMode::Listed, &["1000", "1001"]);
    let refused = p.info(&fake(false, true, "1002"));
    assert_eq!(refused.mode, AccessMode::Listed);
    assert!(!refused.you_allowed);
    assert_eq!(refused.you.id, "1002");
    assert!(
        refused.users.is_none(),
        "refused callers never see the list"
    );
    // Sandboxed administrators are refused too and learn nothing more.
    assert!(p.info(&fake(false, false, "0")).users.is_none());
    // An allowed standard user does not get the list either.
    let listed = p.info(&fake(false, true, "1000"));
    assert!(listed.you_allowed && listed.users.is_none());
    assert_eq!(listed.you.name, "name-1000");
    let admin = p.info(&fake(true, true, "0"));
    assert_eq!(admin.users.as_ref().map(Vec::len), Some(2));
}

#[test]
fn parse_accepts_the_documented_shape_and_round_trips() {
    let text = r#"{"version":1,"mode":"listed","users":[{"id":"S-1-5-21-1-2-3-1001","name":"PC\\alice"},{"id":"1000"}]}"#;
    let p = Policy::parse(text.as_bytes()).unwrap();
    assert_eq!(p.mode, AccessMode::Listed);
    assert_eq!(p.users[0].name, r"PC\alice");
    assert_eq!(p.users[1].name, "");
    assert_eq!(Policy::parse(p.to_json().as_bytes()).unwrap(), p);
}

#[test]
fn parse_rejects_anything_that_could_widen_by_mistake() {
    let many = format!(
        r#"{{"version":1,"mode":"listed","users":[{}]}}"#,
        vec![r#"{"id":"1"}"#; MAX_ACCESS_USERS + 1].join(",")
    );
    for bad in [
        "",
        "not json",
        r#"{"version":2,"mode":"everyone"}"#,
        r#"{"version":1,"mode":"all"}"#,
        r#"{"version":1,"mode":"everyone","extra":1}"#,
        r#"{"version":1,"mode":"listed","users":[{"id":"alice"}]}"#,
        r#"{"version":1,"mode":"listed","users":[{"id":"S-1-x"}]}"#,
        r#"{"version":1,"mode":"listed","users":[{"id":""}]}"#,
        many.as_str(),
    ] {
        assert!(Policy::parse(bad.as_bytes()).is_err(), "{bad}");
    }
    let big = format!(
        r#"{{"version":1,"mode":"everyone","users":[],"x":"{}"}}"#,
        "a".repeat(70_000)
    );
    assert!(Policy::parse(big.as_bytes()).is_err());
}

struct Dir(PathBuf);
impl Dir {
    fn new(tag: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!("ferrow-access-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }
    fn file(&self) -> PathBuf {
        self.0.join(FILE_NAME)
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn accept(_: &Path) -> io::Result<()> {
    Ok(())
}
fn unprotected(_: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "not protected",
    ))
}
fn missing(_: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::NotFound))
}

const EVERYONE: &str = r#"{"version":1,"mode":"everyone"}"#;

#[test]
fn missing_malformed_oversized_and_unprotected_files_mean_administrators_only() {
    let dir = Dir::new("fallback");
    let path = dir.file();
    assert_eq!(load_with(&path, missing).unwrap(), Policy::default());
    for (content, validate) in [
        (EVERYONE.to_owned(), unprotected as Validate),
        ("{ broken".to_owned(), accept),
        (format!("{EVERYONE}{}", " ".repeat(70_000)), accept),
    ] {
        std::fs::write(&path, content).unwrap();
        assert!(load_with(&path, validate).is_err());
        let store = PolicyStore::with(path.clone(), validate, RECHECK);
        assert_eq!(store.current().mode, AccessMode::Admins);
    }
    std::fs::write(&path, EVERYONE).unwrap();
    let store = PolicyStore::with(path, accept, RECHECK);
    assert_eq!(store.current().mode, AccessMode::Everyone);
}

#[test]
fn a_change_applies_after_the_recheck_gate_and_not_before() {
    let dir = Dir::new("reload");
    let path = dir.file();
    let store = PolicyStore::with(path.clone(), accept, Duration::from_millis(300));
    assert_eq!(store.current().mode, AccessMode::Admins);
    std::fs::write(&path, EVERYONE).unwrap();
    assert_eq!(
        store.current().mode,
        AccessMode::Admins,
        "applied inside the gate"
    );
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(store.current().mode, AccessMode::Everyone);
    // Revocation: deleting the file returns to administrators only.
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        store.current().mode,
        AccessMode::Everyone,
        "applied inside the gate"
    );
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(store.current().mode, AccessMode::Admins);
    assert_eq!(RECHECK, Duration::from_secs(1));
}

#[test]
fn the_hot_path_does_not_touch_the_file_between_checks() {
    let dir = Dir::new("hot");
    let path = dir.file();
    std::fs::write(&path, EVERYONE).unwrap();
    let store = PolicyStore::with(path.clone(), accept, Duration::from_secs(3600));
    std::fs::remove_file(&path).unwrap();
    for _ in 0..1000 {
        assert_eq!(store.current().mode, AccessMode::Everyone);
    }
}

#[test]
fn write_is_atomic_leaves_no_temporary_and_a_failed_validation_changes_nothing() {
    let dir = Dir::new("write");
    let path = dir.file();
    let listed = policy(AccessMode::Listed, &["1000"]);
    write_with(&path, &listed, accept, accept).unwrap();
    assert_eq!(load_with(&path, accept).unwrap(), listed);
    let before = std::fs::read(&path).unwrap();
    let everyone = policy(AccessMode::Everyone, &[]);
    assert!(write_with(&path, &everyone, accept, unprotected).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before, "old policy kept");
    let names: Vec<_> = std::fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from(FILE_NAME)]);
}

fn access_info(
    system: bool,
    access: Option<&PolicyStore>,
    caller: &CallerIdentity,
) -> Vec<ferrow_ipc::Response> {
    let store = crate::SnapshotStore::new(crate::Snapshot::empty());
    let owner = CallerIdentity::current().unwrap();
    crate::handle_authorized_cancel(
        &store,
        ferrow_ipc::Request::AccessInfo,
        caller,
        &owner,
        system,
        access,
        ferrow_query::CancelToken::new(),
    )
}

#[test]
fn the_development_endpoint_ignores_the_policy() {
    let dir = Dir::new("dev");
    std::fs::write(dir.file(), EVERYONE).unwrap();
    let store = PolicyStore::with(dir.file(), accept, RECHECK);
    let me = CallerIdentity::current().unwrap();
    // The policy has no say on the development endpoint, not even its info.
    assert!(matches!(
        &access_info(false, Some(&store), &me)[..],
        [ferrow_ipc::Response::Error {
            code: ferrow_ipc::ErrorCode::BadRequest,
            ..
        }]
    ));
    assert!(matches!(
        &access_info(true, Some(&store), &me)[..],
        [ferrow_ipc::Response::Access(i)] if i.mode == AccessMode::Everyone
    ));
}

#[cfg(unix)]
#[test]
fn a_stranger_uid_is_served_by_the_system_endpoint_only_under_everyone() {
    let dir = Dir::new("stranger");
    std::fs::write(dir.file(), EVERYONE).unwrap();
    let store = PolicyStore::with(dir.file(), accept, RECHECK);
    let me = CallerIdentity::current().unwrap();
    let stranger = CallerIdentity {
        uid: Some(me.uid.unwrap() + 1),
        ..CallerIdentity::default()
    };
    let snapshots = crate::SnapshotStore::new(crate::Snapshot::empty());
    let status = |system| {
        crate::handle_authorized_cancel(
            &snapshots,
            ferrow_ipc::Request::Status,
            &stranger,
            &me,
            system,
            Some(&store),
            ferrow_query::CancelToken::new(),
        )
    };
    assert!(matches!(
        &status(true)[..],
        [ferrow_ipc::Response::Status { .. }]
    ));
    assert!(matches!(
        &status(false)[..],
        [ferrow_ipc::Response::Error {
            code: ferrow_ipc::ErrorCode::Unauthorized,
            ..
        }]
    ));
}
