//! Account lookups for the elevated `ferrow service access` commands.
//!
//! Names are resolved here, in the short-lived administrator process, never
//! in the service: a lookup on a domain account can wait for a domain
//! controller, which a request thread must never do.

use ferrow_ipc::AccessUser;
use ferrow_ipc::protocol::MAX_ACCOUNT_NAME;

use crate::access::printable;

/// An account from a name, a SID (Windows) or a uid (Unix). Groups are
/// refused: only a user can sign in, so a group in the list would grant nobody.
pub fn resolve(text: &str) -> Result<AccessUser, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("name an account or give its SID/uid".into());
    }
    platform::resolve(text)
}

/// Whether this process has administrator (Windows, elevated) or root rights.
pub fn is_elevated() -> bool {
    platform::is_elevated()
}

/// The part of `name` that is display text, cut to the wire limit.
fn display(name: String) -> String {
    let mut name = name;
    while name.len() > MAX_ACCOUNT_NAME {
        name.pop();
    }
    name
}

#[cfg(unix)]
mod platform {
    use super::*;

    pub fn is_elevated() -> bool {
        // SAFETY: credential getter without preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    /// `(uid, name)` of a passwd entry, looked up by uid or by name.
    fn entry(by_uid: Option<u32>, name: &str) -> Option<(u32, String)> {
        let name = std::ffi::CString::new(name).ok()?;
        let mut buffer = vec![0u8; 16 * 1024];
        // SAFETY: zeroed passwd is a valid output record.
        let mut record: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: valid record, buffer and result pointer for the call;
        // the name is a NUL-terminated string.
        let rc = unsafe {
            match by_uid {
                Some(uid) => libc::getpwuid_r(
                    uid,
                    &raw mut record,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &raw mut found,
                ),
                None => libc::getpwnam_r(
                    name.as_ptr(),
                    &raw mut record,
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &raw mut found,
                ),
            }
        };
        if rc != 0 || found.is_null() || record.pw_name.is_null() {
            return None;
        }
        // SAFETY: pw_name points at a NUL-terminated string inside `buffer`.
        let user = unsafe { std::ffi::CStr::from_ptr(record.pw_name) };
        Some((record.pw_uid, user.to_string_lossy().into_owned()))
    }

    pub fn resolve(text: &str) -> Result<AccessUser, String> {
        if text.bytes().all(|b| b.is_ascii_digit()) {
            let uid: u32 = text
                .parse()
                .map_err(|_| format!("not a valid uid: {}", printable(text)))?;
            let name = entry(Some(uid), "").map(|(_, n)| n).unwrap_or_default();
            return Ok(AccessUser {
                id: uid.to_string(),
                name: display(name),
            });
        }
        match entry(None, text) {
            Some((uid, name)) => Ok(AccessUser {
                id: uid.to_string(),
                name: display(name),
            }),
            None => Err(format!("no such user: {}", printable(text))),
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use crate::access::valid_id;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSidToSidW,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, LookupAccountNameW, LookupAccountSidW, PSID, SidTypeUser,
        TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    pub fn is_elevated() -> bool {
        let mut token = std::ptr::null_mut();
        // SAFETY: current-process pseudo handle; the token is closed below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0;
        // SAFETY: live token and a correctly sized output structure.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                (&raw mut elevation).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &raw mut returned,
            )
        };
        // SAFETY: the token handle is owned here and closed once.
        unsafe { CloseHandle(token) };
        ok != 0 && elevation.TokenIsElevated != 0
    }

    fn sid_text(sid: PSID) -> Option<String> {
        let mut text = std::ptr::null_mut();
        // SAFETY: a valid SID; the allocated string is freed below.
        if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
            return None;
        }
        // SAFETY: NUL-terminated string owned until LocalFree.
        let out = unsafe {
            let mut len = 0;
            while *text.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
        };
        // SAFETY: frees the string allocated by the conversion.
        unsafe { LocalFree(text.cast()) };
        Some(out)
    }

    /// `(DOMAIN\name, is_user)` for a SID, when Windows can resolve it.
    fn describe(sid: &str) -> Option<(String, bool)> {
        let text = wide(sid);
        let mut psid: PSID = std::ptr::null_mut();
        // SAFETY: NUL-terminated input; the returned SID is freed below.
        if unsafe { ConvertStringSidToSidW(text.as_ptr(), &raw mut psid) } == 0 {
            return None;
        }
        let (mut name, mut domain) = ([0u16; 512], [0u16; 512]);
        let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
        let mut kind = 0;
        // SAFETY: valid SID and buffers whose lengths are passed in.
        let ok = unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                psid,
                name.as_mut_ptr(),
                &raw mut name_len,
                domain.as_mut_ptr(),
                &raw mut domain_len,
                &raw mut kind,
            )
        };
        // SAFETY: frees the SID allocated by the conversion above.
        unsafe { LocalFree(psid.cast()) };
        if ok == 0 {
            return None;
        }
        let name = String::from_utf16_lossy(&name[..name_len as usize]);
        let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
        let full = if domain.is_empty() {
            name
        } else {
            format!("{domain}\\{name}")
        };
        Some((full, kind == SidTypeUser))
    }

    fn sid_of_name(name: &str) -> Result<String, String> {
        let text = wide(name);
        let mut sid = [0usize; 16]; // 128 bytes, above the 68-byte SID maximum
        let mut sid_len = std::mem::size_of_val(&sid) as u32;
        let mut domain = [0u16; 512];
        let mut domain_len = domain.len() as u32;
        let mut kind = 0;
        // SAFETY: NUL-terminated name and buffers whose lengths are passed in.
        let ok = unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                text.as_ptr(),
                sid.as_mut_ptr().cast(),
                &raw mut sid_len,
                domain.as_mut_ptr(),
                &raw mut domain_len,
                &raw mut kind,
            )
        };
        if ok == 0 {
            return Err(format!("no such account: {}", printable(name)));
        }
        sid_text(sid.as_mut_ptr().cast()).ok_or_else(|| "cannot read the account SID".into())
    }

    pub fn resolve(text: &str) -> Result<AccessUser, String> {
        let id = if text
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("S-1-"))
        {
            if !valid_id(&text.to_ascii_uppercase()) {
                return Err(format!("not a valid SID: {}", printable(text)));
            }
            text.to_ascii_uppercase()
        } else {
            sid_of_name(text)?
        };
        match describe(&id) {
            Some((_, false)) => Err(format!(
                "{} is not a user account; add individual users, or use `everyone`",
                printable(text)
            )),
            Some((name, true)) => Ok(AccessUser {
                id,
                name: display(name),
            }),
            // An unresolvable SID (for example a deleted account) is stored
            // as given; it matches nobody who is signed in.
            None => Ok(AccessUser {
                id,
                name: String::new(),
            }),
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;
    pub fn is_elevated() -> bool {
        false
    }
    pub fn resolve(_: &str) -> Result<AccessUser, String> {
        Err("account lookup is not supported on this platform".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_ascii_and_odd_names_never_panic() {
        // A multi-byte name used to be sliced inside a character.
        for text in [
            "山田太郎",
            "S-1-山田",
            "山S-1-5-21-1-2-3-1001",
            "é",
            "S-1",
            "S",
            "\u{1b}[31m",
            "   ",
            "",
        ] {
            let _ = resolve(text);
        }
        assert!(resolve("山田太郎").is_err(), "no such account");
        assert!(resolve("").is_err());
    }

    #[test]
    fn errors_never_echo_control_characters() {
        let error = resolve("x\u{1b}[2Jy").unwrap_err();
        assert!(!error.contains('\u{1b}'), "{error:?}");
    }
}
