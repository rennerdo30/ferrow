use super::*;
use std::sync::atomic::AtomicBool;

struct Fake<'a> {
    admin: bool,
    unsandboxed: bool,
    person: bool,
    id: &'a str,
}
impl Caller for Fake<'_> {
    fn admin(&self) -> bool {
        self.admin
    }
    fn unsandboxed(&self) -> bool {
        self.unsandboxed
    }
    fn person(&self) -> bool {
        self.person
    }
    fn id(&self) -> Option<String> {
        Some(self.id.into())
    }
}

const fn fake(admin: bool, unsandboxed: bool, person: bool, id: &str) -> Fake<'_> {
    Fake {
        admin,
        unsandboxed,
        person,
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
fn matrix(system: &str, admin: &str, listed: &str, unlisted: &str, service: &str) {
    //                  admin  unsandboxed person
    let callers = [
        ("system", fake(true, true, false, system)),
        ("admin", fake(true, true, true, admin)),
        ("standard listed", fake(false, true, true, listed)),
        ("standard unlisted", fake(false, true, true, unlisted)),
        ("service account", fake(false, true, false, service)),
        ("sandboxed admin", fake(false, false, false, admin)),
        ("sandboxed listed", fake(false, false, false, listed)),
    ];
    // Expected answers in the modes: admins, everyone, listed. The listed
    // service account is the last case in the table below.
    let expect = [
        [true, true, true],
        [true, true, true],
        [false, true, true],
        [false, true, false],
        [false, false, false],
        [false, false, false],
        [false, false, false],
    ];
    let modes = [AccessMode::Admins, AccessMode::Everyone, AccessMode::Listed];
    for ((name, caller), row) in callers.iter().zip(expect) {
        for (mode, want) in modes.into_iter().zip(row) {
            let p = policy(mode, &[listed]);
            assert_eq!(p.allows(caller), want, "{name} in {mode:?}");
        }
    }
    // `listed` admits exactly the chosen ids, so a listed service account is
    // admitted there (an administrator named it) but not under `everyone`.
    let chosen = fake(false, true, false, service);
    assert!(policy(AccessMode::Listed, &[service]).allows(&chosen));
    assert!(!policy(AccessMode::Everyone, &[service]).allows(&chosen));
}

#[test]
fn decision_matrix_windows_sids() {
    matrix(
        "S-1-5-18",
        "S-1-5-21-1-2-3-500",
        "S-1-5-21-1-2-3-1001",
        "S-1-5-21-1-2-3-1002",
        "S-1-5-80-1-2-3-4-5",
    );
}

#[test]
fn decision_matrix_unix_uids() {
    matrix("0", "0", "1000", "1001", "999");
}

#[test]
fn a_listed_standard_user_is_refused_by_the_administrators_only_rule() {
    // The rule before the policy was `admin()` alone: it refuses this caller.
    let listed = fake(false, true, true, "S-1-5-21-1-2-3-1001");
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
fn the_real_caller_impl_wires_admin_unsandboxed_and_person_to_the_identity() {
    let me = CallerIdentity::current().unwrap();
    assert_eq!(Caller::admin(&me), me.is_admin_account());
    assert_eq!(Caller::unsandboxed(&me), me.is_unsandboxed());
    assert_eq!(Caller::person(&me), me.is_person());
    assert_eq!(Caller::id(&me), me.account_id());
    assert!(me.is_unsandboxed(), "an ordinary process of this session");
    assert!(me.account_id().is_some_and(|id| valid_id(&id)));
    if me.is_admin_account() {
        assert!(me.is_unsandboxed(), "administrators are unsandboxed");
    }
    // GitHub's Windows runner is an elevated administrator.
    if cfg!(windows) && std::env::var_os("GITHUB_ACTIONS").is_some() {
        assert!(Caller::admin(&me) && Caller::unsandboxed(&me));
        assert!(policy(AccessMode::Admins, &[]).allows(&me));
        assert!(policy(AccessMode::Everyone, &[]).allows(&me));
    }
    // The development process is never refused for lack of identity.
    let listed = Policy {
        mode: AccessMode::Listed,
        users: vec![AccessUser {
            id: me.account_id().unwrap(),
            name: String::new(),
        }],
    };
    assert!(listed.allows(&me));
}

#[test]
fn access_info_tells_a_refused_caller_only_mode_verdict_and_own_identity() {
    let p = policy(AccessMode::Listed, &["1000", "1001"]);
    let refused = p.info(&fake(false, true, true, "1002"));
    assert_eq!(refused.mode, AccessMode::Listed);
    assert!(!refused.you_allowed);
    assert_eq!(refused.you.id, "1002");
    assert!(
        refused.users.is_none(),
        "refused callers never see the list"
    );
    // Sandboxed administrators are refused too and learn nothing more.
    assert!(p.info(&fake(false, false, false, "0")).users.is_none());
    // An allowed standard user does not get the list either.
    let listed = p.info(&fake(false, true, true, "1000"));
    assert!(listed.you_allowed && listed.users.is_none());
    assert_eq!(listed.you.name, "name-1000");
    let admin = p.info(&fake(true, true, true, "0"));
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
fn a_leading_utf8_byte_order_mark_is_accepted() {
    let mut bytes = b"\xEF\xBB\xBF".to_vec();
    bytes.extend_from_slice(EVERYONE.as_bytes());
    assert_eq!(Policy::parse(&bytes).unwrap().mode, AccessMode::Everyone);
    // Only one, and only at the start.
    let mut twice = b"\xEF\xBB\xBF\xEF\xBB\xBF".to_vec();
    twice.extend_from_slice(EVERYONE.as_bytes());
    assert!(Policy::parse(&twice).is_err());
}

#[test]
fn ids_are_canonical_sids_or_canonical_uids() {
    for good in [
        "S-1-5-18",
        "S-1-5-21-1-2-3-1001",
        "S-1-5-32-544",
        "0",
        "1000",
        "4294967295",
    ] {
        assert!(valid_id(good), "{good}");
    }
    for bad in [
        "",
        "S-1-",
        "S-1",
        "S-1-5",
        "S-1-5-",
        "S-1--5",
        "S-1-5--18",
        "S-1-5-18-",
        "S-1-x-18",
        "S-2-5-18",
        "s-1-5-18",
        "01000",
        "-1",
        "4294967296",
        "1000 ",
        "S-1-5-1-2-3-4-5-6-7-8-9-10-11-12-13-14-15-16",
        "S-1-5-99999999999",
    ] {
        assert!(!valid_id(bad), "{bad:?}");
    }
}

#[test]
fn control_characters_are_escaped_when_names_are_printed() {
    assert_eq!(printable("alice"), "alice");
    assert_eq!(printable("山田"), "山田");
    let shown = printable("a\u{1b}[31mb\n\u{7}");
    assert!(!shown.chars().any(char::is_control), "{shown:?}");
    assert!(shown.contains("\\u{1b}") && shown.contains("\\u{a}"));
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

/// A store on a clock the test moves by hand.
fn store(path: &Path, validate: Validate, interval_ms: u64) -> (PolicyStore, Arc<AtomicU64>) {
    let now = Arc::new(AtomicU64::new(10_000));
    let reader = now.clone();
    let clock: Clock = Arc::new(move || reader.load(Ordering::Relaxed));
    let store = PolicyStore::with(
        path.to_path_buf(),
        validate,
        Duration::from_millis(interval_ms),
        clock,
    );
    (store, now)
}

fn advance(now: &AtomicU64, ms: u64) {
    now.fetch_add(ms, Ordering::Relaxed);
}

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
        let (store, _) = store(&path, validate, 1000);
        assert_eq!(store.current().mode, AccessMode::Admins);
    }
    std::fs::write(&path, EVERYONE).unwrap();
    assert_eq!(
        store(&path, accept, 1000).0.current().mode,
        AccessMode::Everyone
    );
}

#[test]
fn a_change_applies_at_the_recheck_gate_and_not_before() {
    let dir = Dir::new("reload");
    let path = dir.file();
    let (store, now) = store(&path, accept, 1000);
    assert_eq!(store.current().mode, AccessMode::Admins);
    std::fs::write(&path, EVERYONE).unwrap();
    advance(&now, 999);
    assert_eq!(store.current().mode, AccessMode::Admins, "inside the gate");
    advance(&now, 1);
    assert_eq!(store.current().mode, AccessMode::Everyone, "at the gate");
    // Revocation: deleting the file returns to administrators only.
    std::fs::remove_file(&path).unwrap();
    advance(&now, 999);
    assert_eq!(
        store.current().mode,
        AccessMode::Everyone,
        "inside the gate"
    );
    advance(&now, 1);
    assert_eq!(store.current().mode, AccessMode::Admins);
    assert_eq!(RECHECK, Duration::from_secs(1));
}

#[test]
fn the_hot_path_does_not_touch_the_file_between_checks() {
    let dir = Dir::new("hot");
    let path = dir.file();
    std::fs::write(&path, EVERYONE).unwrap();
    let (store, now) = store(&path, accept, 3_600_000);
    std::fs::remove_file(&path).unwrap();
    for _ in 0..1000 {
        advance(&now, 1);
        assert_eq!(store.current().mode, AccessMode::Everyone);
    }
}

static FAIL_ONCE: AtomicBool = AtomicBool::new(true);
fn fails_once(_: &Path) -> io::Result<()> {
    if FAIL_ONCE.swap(false, Ordering::SeqCst) {
        Err(io::Error::other("transient failure"))
    } else {
        Ok(())
    }
}

#[test]
fn a_failed_check_is_retried_and_the_policy_recovers_on_an_unchanged_file() {
    let dir = Dir::new("sticky");
    let path = dir.file();
    std::fs::write(&path, EVERYONE).unwrap();
    // The first validation (at construction) fails: administrators only.
    let (store, now) = store(&path, fails_once, 1000);
    assert_eq!(store.current().mode, AccessMode::Admins);
    // The file did not change, but the next tick validates again and recovers.
    advance(&now, 1000);
    assert_eq!(store.current().mode, AccessMode::Everyone);
}

static PROTECTED: AtomicBool = AtomicBool::new(true);
fn follows_protection(_: &Path) -> io::Result<()> {
    if PROTECTED.load(Ordering::SeqCst) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "grants changed",
        ))
    }
}

#[test]
fn an_acl_change_on_an_unchanged_file_applies_on_the_next_tick_both_ways() {
    let dir = Dir::new("acl");
    let path = dir.file();
    std::fs::write(&path, EVERYONE).unwrap();
    let (store, now) = store(&path, follows_protection, 1000);
    assert_eq!(store.current().mode, AccessMode::Everyone);
    PROTECTED.store(false, Ordering::SeqCst);
    advance(&now, 1000);
    assert_eq!(
        store.current().mode,
        AccessMode::Admins,
        "a file that stopped being protected is no longer trusted"
    );
    PROTECTED.store(true, Ordering::SeqCst);
    advance(&now, 1000);
    assert_eq!(store.current().mode, AccessMode::Everyone);
}

#[test]
fn a_bad_file_is_fixed_by_replacing_it() {
    let dir = Dir::new("fixed");
    let path = dir.file();
    std::fs::write(&path, "{ broken").unwrap();
    let (store, now) = store(&path, accept, 1000);
    assert_eq!(store.current().mode, AccessMode::Admins);
    advance(&now, 1000);
    assert_eq!(
        store.current().mode,
        AccessMode::Admins,
        "still bad, still safe"
    );
    std::fs::write(&path, EVERYONE).unwrap();
    advance(&now, 1000);
    assert_eq!(store.current().mode, AccessMode::Everyone);
}

static SLOW: AtomicBool = AtomicBool::new(false);
fn slow_validation(_: &Path) -> io::Result<()> {
    if SLOW.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1500));
    }
    Ok(())
}

#[test]
fn a_slow_validation_never_delays_other_requests() {
    let dir = Dir::new("slow");
    let path = dir.file();
    std::fs::write(&path, EVERYONE).unwrap();
    let (store, now) = store(&path, slow_validation, 1000);
    SLOW.store(true, Ordering::SeqCst);
    advance(&now, 1000);
    std::thread::scope(|scope| {
        let checker = scope.spawn(|| store.current().mode); // wins the check, waits 1.5 s
        std::thread::sleep(Duration::from_millis(200));
        advance(&now, 1000); // the next check is due while the first still runs
        let started = Instant::now();
        let mode = store.current().mode; // check is due, but the lock is taken
        assert_eq!(mode, AccessMode::Everyone, "the policy in force, at once");
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "waited {:?}",
            started.elapsed()
        );
        assert_eq!(checker.join().unwrap(), AccessMode::Everyone);
    });
    SLOW.store(false, Ordering::SeqCst);
}

fn stamp_of(path: &Path) -> FileStamp {
    FileStamp::of(path)
}

#[test]
fn write_is_atomic_leaves_no_temporary_and_a_failed_validation_changes_nothing() {
    let dir = Dir::new("write");
    let path = dir.file();
    let listed = policy(AccessMode::Listed, &["1000"]);
    write_with(&path, &listed, &stamp_of(&path), accept, accept, accept).unwrap();
    assert_eq!(load_with(&path, accept).unwrap(), listed);
    let before = std::fs::read(&path).unwrap();
    let everyone = policy(AccessMode::Everyone, &[]);
    assert!(
        write_with(
            &path,
            &everyone,
            &stamp_of(&path),
            accept,
            accept,
            unprotected
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "old policy kept");
    let names: Vec<_> = std::fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from(FILE_NAME)]);
}

#[test]
fn write_refuses_what_the_loader_would_refuse_and_touches_no_file() {
    let dir = Dir::new("toobig");
    let path = dir.file();
    let mut big = policy(AccessMode::Listed, &[]);
    big.users = (0..MAX_ACCESS_USERS)
        .map(|n| AccessUser {
            id: format!("{}", 2000 + n),
            name: "n".repeat(MAX_ACCOUNT_NAME),
        })
        .collect();
    assert!(big.to_json().len() as u64 > MAX_POLICY_BYTES);
    let error = write_with(&path, &big, &stamp_of(&path), accept, accept, accept).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(!path.exists());
    assert_eq!(
        std::fs::read_dir(&dir.0).unwrap().count(),
        0,
        "no temporary"
    );
}

#[test]
fn a_write_after_someone_else_changed_the_file_is_refused() {
    let dir = Dir::new("lost");
    let path = dir.file();
    let first = policy(AccessMode::Listed, &["1000"]);
    write_with(&path, &first, &stamp_of(&path), accept, accept, accept).unwrap();
    // Two administrators read the same file...
    let (mine, _) = load_for_update_with(&path);
    let (theirs, _) = load_for_update_with(&path);
    // ...theirs lands first...
    let second = policy(AccessMode::Listed, &["1000", "1001"]);
    write_with(&path, &second, &theirs, accept, accept, accept).unwrap();
    // ...so mine, made from the old content, must not overwrite it.
    let mine_policy = policy(AccessMode::Listed, &["1000", "1002"]);
    let error = write_with(&path, &mine_policy, &mine, accept, accept, accept).unwrap_err();
    assert!(error.to_string().contains("changed meanwhile"), "{error}");
    assert_eq!(
        load_with(&path, accept).unwrap(),
        second,
        "their change survived"
    );
    assert_eq!(
        std::fs::read_dir(&dir.0).unwrap().count(),
        1,
        "no temporary"
    );
}

fn load_for_update_with(path: &Path) -> (FileStamp, Result<Policy, String>) {
    (FileStamp::of(path), load_with(path, accept))
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
    let (store, _) = store(&dir.file(), accept, 1000);
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
    let (store, _) = store(&dir.file(), accept, 1000);
    let me = CallerIdentity::current().unwrap();
    // A regular account other than this one (above every platform's floor).
    let stranger = CallerIdentity {
        uid: Some(me.uid.unwrap().max(20_000) + 1),
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
    // A system account (below the first regular uid) is not a person.
    let service = CallerIdentity {
        uid: Some(5),
        ..CallerIdentity::default()
    };
    let refused = crate::handle_authorized_cancel(
        &snapshots,
        ferrow_ipc::Request::Status,
        &service,
        &me,
        true,
        Some(&store),
        ferrow_query::CancelToken::new(),
    );
    assert!(matches!(
        &refused[..],
        [ferrow_ipc::Response::Error {
            code: ferrow_ipc::ErrorCode::Unauthorized,
            ..
        }]
    ));
}

/// The real protection checks accept what `write` produces, in a real
/// protected folder. Needs an elevated administrator, so it runs on the
/// elevated GitHub Windows runner only (never on a developer PC).
#[cfg(windows)]
#[test]
fn the_real_state_file_checks_accept_what_write_produces() {
    if std::env::var_os("GITHUB_ACTIONS").is_none() || !crate::accounts::is_elevated() {
        eprintln!("skipped: needs an elevated administrator on the CI runner");
        return;
    }
    let base = crate::protection::windows::known_folder(
        &windows_sys::Win32::UI::Shell::FOLDERID_ProgramData,
    )
    .unwrap();
    let dir = base.join(format!("Ferrow-test-{}", std::process::id()));
    let path = dir.join(FILE_NAME);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let listed = policy(AccessMode::Listed, &["S-1-5-21-1-2-3-1001"]);
        let (stamp, first) = load_for_update(&path);
        assert_eq!(
            first,
            Ok(Policy::default()),
            "no folder yet means administrators only"
        );
        write(&path, &listed, &stamp).unwrap();
        // The real loader (owner Administrators or SYSTEM, only their grants).
        assert_eq!(load_file(&path), Ok(listed.clone()));
        let (stamp, again) = load_for_update(&path);
        assert_eq!(again, Ok(listed.clone()));
        let admins = Policy::default();
        write(&path, &admins, &stamp).unwrap();
        assert_eq!(load_file(&path), Ok(admins));
        // And the live store reads it with the same checks.
        let (live, now) = {
            let now = Arc::new(AtomicU64::new(0));
            let reader = now.clone();
            let clock: Clock = Arc::new(move || reader.load(Ordering::Relaxed));
            let store =
                PolicyStore::with(path.clone(), crate::protection::state_file, RECHECK, clock);
            (store, now)
        };
        assert_eq!(live.current().mode, AccessMode::Admins);
        let (stamp, _) = load_for_update(&path);
        write(&path, &policy(AccessMode::Everyone, &[]), &stamp).unwrap();
        advance(&now, 1000);
        assert_eq!(live.current().mode, AccessMode::Everyone);
    }));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
