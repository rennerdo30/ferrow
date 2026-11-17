//! Which callers count as a person who signs in to the PC (the `everyone`
//! access mode). Pure rules, so both platforms' rules are tested everywhere;
//! `transport` feeds them the OS facts.

// Each platform uses only its own half.
#![allow(dead_code)]

/// `SE_GROUP_ENABLED` in a token group's attributes.
pub const GROUP_ENABLED: u32 = 0x4;

/// Windows: the token holds the INTERACTIVE group (S-1-5-4: console and
/// remote-desktop sign-ins; services and network logons never have it) as an
/// enabled group, is not in Guests (S-1-5-32-546, in any form) and is not the
/// Guest account itself (relative id 501). Sandbox checks are separate.
pub fn windows_person(
    user_sid: Option<&str>,
    interactive_attributes: Option<u32>,
    guests_attributes: Option<u32>,
) -> bool {
    let guest_account = user_sid.is_none_or(|sid| sid.ends_with("-501"));
    interactive_attributes.is_some_and(|a| a & GROUP_ENABLED != 0)
        && guests_attributes.is_none()
        && !guest_account
}

/// Unix: a regular account, at or above the platform's first regular uid,
/// and not `nobody` (65534, or -2 on macOS) or the invalid uid -1.
pub fn unix_person(uid: u32, first_regular: u32) -> bool {
    uid >= first_regular && uid != 65534 && uid < u32::MAX - 1
}

/// `UID_MIN` from `login.defs` text: the first uncommented `UID_MIN <number>`.
pub fn parse_uid_min(text: &str) -> Option<u32> {
    text.lines().find_map(|line| {
        let mut words = line.split('#').next()?.split_whitespace();
        (words.next()? == "UID_MIN").then(|| words.next()?.parse().ok())?
    })
}

/// The first regular uid: Linux `UID_MIN` (default 1000), macOS 501.
pub fn first_regular_uid(login_defs: Option<&str>) -> u32 {
    if cfg!(target_os = "macos") {
        return 501;
    }
    login_defs.and_then(parse_uid_min).unwrap_or(1000).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_people_sign_in_interactively_and_are_not_guests() {
        let person = |sid, interactive, guests| windows_person(sid, interactive, guests);
        let user = Some("S-1-5-21-1-2-3-1001");
        assert!(person(user, Some(0x7), None), "an interactive sign-in");
        assert!(
            !person(user, None, None),
            "service or network logon: no INTERACTIVE"
        );
        assert!(
            !person(user, Some(0x10), None),
            "INTERACTIVE only deny-only"
        );
        assert!(!person(user, Some(0x7), Some(0x7)), "member of Guests");
        assert!(
            !person(user, Some(0x7), Some(0x10)),
            "deny-only Guests still counts as Guests"
        );
        assert!(
            !person(Some("S-1-5-21-1-2-3-501"), Some(0x7), None),
            "the Guest account"
        );
        assert!(
            !person(Some("S-1-5-18"), None, None),
            "SYSTEM has no sign-in (it is admitted as SYSTEM)"
        );
        assert!(!person(None, Some(0x7), None), "no user sid");
    }

    #[test]
    fn unix_people_are_regular_uids_and_not_nobody() {
        assert!(unix_person(1000, 1000) && unix_person(5000, 1000));
        assert!(!unix_person(999, 1000), "system account below the floor");
        assert!(
            !unix_person(0, 1000),
            "root is admitted as root, not as a person"
        );
        assert!(!unix_person(65534, 1000), "nobody");
        assert!(!unix_person(u32::MAX - 1, 501), "macOS nobody (-2)");
        assert!(!unix_person(u32::MAX, 501), "invalid uid");
        assert!(unix_person(501, 501) && !unix_person(500, 501));
    }

    #[test]
    fn uid_min_comes_from_login_defs_with_a_default() {
        assert_eq!(
            parse_uid_min("UID_MIN\t\t1000\nUID_MAX 60000\n"),
            Some(1000)
        );
        assert_eq!(parse_uid_min("# UID_MIN 5\nUID_MIN 500 # old\n"), Some(500));
        assert_eq!(parse_uid_min("SYS_UID_MIN 100\nUID_MIN notanumber\n"), None);
        assert_eq!(parse_uid_min(""), None);
        if !cfg!(target_os = "macos") {
            assert_eq!(first_regular_uid(None), 1000);
            assert_eq!(first_regular_uid(Some("UID_MIN 1500")), 1500);
            assert_eq!(first_regular_uid(Some("UID_MIN 0")), 1, "never admits root");
        } else {
            assert_eq!(first_regular_uid(Some("UID_MIN 1500")), 501);
        }
    }
}
