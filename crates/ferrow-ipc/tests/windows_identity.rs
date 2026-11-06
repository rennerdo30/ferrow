#![cfg(windows)]
//! A broker keeps its process token, but opens the pipe with a restricted
//! thread token. Authorization must retain that connecting security context.
use ferrow_ipc::{CallerIdentity, Endpoint, Listener, Stream};
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::{Foundation::HANDLE, Security::*, System::Threading::SetThreadToken};

fn duplicate(token: HANDLE) -> OwnedHandle {
    let mut copy = std::ptr::null_mut();
    // SAFETY: live input token and output; ownership transfers on success.
    assert_ne!(
        unsafe { DuplicateToken(token, SecurityImpersonation, &raw mut copy) },
        0
    );
    // SAFETY: newly returned token handle is uniquely owned.
    unsafe { OwnedHandle::from_raw_handle(copy) }
}

fn member(token: HANDLE, sid: PSID) -> bool {
    let mut enabled = 0;
    // SAFETY: live impersonation token, valid SID and live output.
    assert_ne!(
        unsafe { CheckTokenMembership(token, sid, &raw mut enabled) },
        0
    );
    enabled != 0
}

struct Revert;
impl Drop for Revert {
    fn drop(&mut self) {
        // SAFETY: guard is created only following this thread's SetThreadToken.
        if unsafe { RevertToSelf() } == 0 {
            std::process::abort();
        }
    }
}

#[test]
fn restricted_broker_context_survives_the_primary_process_token() {
    let mut world = [0usize; 16];
    let mut bytes = std::mem::size_of_val(&world) as u32;
    // SAFETY: aligned buffer is larger than SECURITY_MAX_SID_SIZE.
    assert_ne!(
        unsafe {
            CreateWellKnownSid(
                WinWorldSid,
                std::ptr::null_mut(),
                world.as_mut_ptr().cast(),
                &raw mut bytes,
            )
        },
        0
    );
    let primary = CallerIdentity::current().unwrap();
    let primary = primary.access_token().unwrap();
    assert!(member(
        duplicate(primary.as_raw_handle()).as_raw_handle(),
        world.as_mut_ptr().cast()
    ));
    let disabled = SID_AND_ATTRIBUTES {
        Sid: world.as_mut_ptr().cast(),
        Attributes: 0,
    };
    let mut restricted = std::ptr::null_mut();
    // SAFETY: disable one existing group; all other arrays are empty. Returned
    // token is owned and cannot elevate the current process.
    assert_ne!(
        unsafe {
            CreateRestrictedToken(
                primary.as_raw_handle(),
                0,
                1,
                &disabled,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &raw mut restricted,
            )
        },
        0
    );
    // SAFETY: newly returned token handle is uniquely owned.
    let restricted = unsafe { OwnedHandle::from_raw_handle(restricted) };
    let restricted = duplicate(restricted.as_raw_handle());
    assert!(!member(
        restricted.as_raw_handle(),
        world.as_mut_ptr().cast()
    ));

    let endpoint = Endpoint::named(&format!("fr-restricted-{}", std::process::id()));
    let listener = Listener::bind(&endpoint).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.read_exact(&mut [0u8]).unwrap();
        let token = stream.caller().unwrap().access_token().unwrap();
        let mut world = [0usize; 16];
        let mut bytes = std::mem::size_of_val(&world) as u32;
        // SAFETY: same correctly aligned, sufficiently sized SID buffer.
        assert_ne!(
            unsafe {
                CreateWellKnownSid(
                    WinWorldSid,
                    std::ptr::null_mut(),
                    world.as_mut_ptr().cast(),
                    &raw mut bytes,
                )
            },
            0
        );
        assert!(
            !member(token.as_raw_handle(), world.as_mut_ptr().cast()),
            "server used the broker's unrestricted primary token"
        );
    });
    // SAFETY: null thread argument means this thread; duplicate is a live
    // restricted impersonation token derived from this same logon identity.
    assert_ne!(
        unsafe { SetThreadToken(std::ptr::null(), restricted.as_raw_handle()) },
        0
    );
    let guard = Revert;
    let mut client = Stream::connect(&endpoint).unwrap();
    client.write_all(&[1]).unwrap();
    drop(guard); // static tracking must retain the opener even after reverting
    server.join().unwrap();
}
