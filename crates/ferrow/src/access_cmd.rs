//! `ferrow service access show | admins | everyone | add <who> | remove <who>`.
//!
//! `show` asks the service (any account may). The other commands change the
//! protected policy file and need administrator rights; Ferrow never elevates
//! itself, so without them the command says what to re-run.

use std::ffi::OsString;
use std::time::{Duration, Instant};

use ferrow_ipc::protocol::MAX_ACCESS_USERS;
use ferrow_ipc::{AccessInfo, AccessMode, AccessUser, Client, Endpoint};
use ferrowd::access::{self, Policy};

pub const USAGE: &str = "ferrow service access show | admins | everyone | add <account-or-SID> | remove <account-or-SID>";

#[derive(Debug, PartialEq, Eq)]
pub enum Cmd {
    Show,
    Admins,
    Everyone,
    Add(String),
    Remove(String),
}

pub fn parse(args: &[OsString]) -> Result<Cmd, String> {
    let words: Vec<&str> = args
        .iter()
        .map(|a| a.to_str().ok_or("arguments must be valid UTF-8"))
        .collect::<Result<_, _>>()?;
    match words.as_slice() {
        [] | ["show"] => Ok(Cmd::Show),
        ["admins"] => Ok(Cmd::Admins),
        ["everyone"] => Ok(Cmd::Everyone),
        ["add", who] => Ok(Cmd::Add((*who).into())),
        ["remove", who] => Ok(Cmd::Remove((*who).into())),
        _ => Err(format!("usage: {USAGE}")),
    }
}

fn who(user: &AccessUser) -> String {
    if user.name.is_empty() {
        user.id.clone()
    } else {
        format!("{} ({})", user.name, user.id)
    }
}

/// The policy after `cmd`, and what to tell the administrator. Modes keep
/// the list tidy: only `listed` holds users, `admins` and `everyone` hold none,
/// and removing the last listed user returns to administrators only.
pub fn apply(
    current: &Policy,
    cmd: &Cmd,
    resolve: &dyn Fn(&str) -> Result<AccessUser, String>,
) -> Result<(Policy, String), String> {
    let policy = |mode, users| Policy { mode, users };
    match cmd {
        Cmd::Show => Ok((current.clone(), String::new())),
        Cmd::Admins => Ok((
            policy(AccessMode::Admins, vec![]),
            "Only administrators and SYSTEM/root can use the service.".into(),
        )),
        Cmd::Everyone => Ok((
            policy(AccessMode::Everyone, vec![]),
            "Every account on this PC can now use the service and see every file name on it."
                .into(),
        )),
        Cmd::Add(name) => {
            let user = resolve(name)?;
            if current.mode == AccessMode::Everyone {
                return Ok((
                    current.clone(),
                    "Every user is already allowed; nothing to add.".into(),
                ));
            }
            let mut users = current.users.clone();
            users.retain(|u| !u.id.eq_ignore_ascii_case(&user.id));
            users.push(user.clone());
            if users.len() > MAX_ACCESS_USERS {
                return Err(format!("at most {MAX_ACCESS_USERS} users can be listed"));
            }
            Ok((
                policy(AccessMode::Listed, users),
                format!("{} can now use the service.", who(&user)),
            ))
        }
        Cmd::Remove(name) => {
            if current.mode != AccessMode::Listed {
                return Err(format!(
                    "no users are listed (the mode is {})",
                    current.mode.as_str()
                ));
            }
            // A listed id or name matches without a lookup, so an account
            // that no longer exists can still be removed.
            let by_text = |u: &AccessUser| {
                u.id.eq_ignore_ascii_case(name)
                    || (!u.name.is_empty() && u.name.eq_ignore_ascii_case(name))
            };
            let found = current.users.iter().position(by_text).or_else(|| {
                let id = resolve(name).ok()?.id;
                current
                    .users
                    .iter()
                    .position(|u| u.id.eq_ignore_ascii_case(&id))
            });
            let Some(at) = found else {
                return Err(format!("{name} is not in the list"));
            };
            let mut users = current.users.clone();
            let removed = users.remove(at);
            let mode = if users.is_empty() {
                AccessMode::Admins
            } else {
                AccessMode::Listed
            };
            Ok((
                policy(mode, users),
                format!("{} can no longer use the service.", who(&removed)),
            ))
        }
    }
}

fn label(mode: AccessMode) -> &'static str {
    match mode {
        AccessMode::Admins => "administrators only",
        AccessMode::Everyone => "every user on this PC",
        AccessMode::Listed => "administrators and the listed users",
    }
}

pub fn format_info(info: &AccessInfo) -> String {
    let mut text = format!("access: {}\n", label(info.mode));
    let verdict = if info.you_allowed {
        "answered"
    } else {
        "not answered"
    };
    text.push_str(&format!("this account: {} - {verdict}\n", who(&info.you)));
    if let Some(users) = &info.users {
        if users.is_empty() {
            text.push_str("listed users: none\n");
        }
        for user in users {
            text.push_str(&format!("listed user: {}\n", who(user)));
        }
    }
    text
}

fn needs_elevation(cmd: &Cmd) -> String {
    let args = match cmd {
        Cmd::Admins => "admins".to_owned(),
        Cmd::Everyone => "everyone".to_owned(),
        Cmd::Add(who) => format!("add {who}"),
        Cmd::Remove(who) => format!("remove {who}"),
        Cmd::Show => "show".to_owned(),
    };
    if cfg!(windows) {
        format!(
            "changing who may use the service needs administrator rights.\n\
             Re-run `ferrow service access {args}` from an elevated terminal. Ferrow never elevates itself."
        )
    } else {
        format!(
            "changing who may use the service needs root.\n\
             Re-run with sudo: sudo ferrow service access {args}. Ferrow never elevates itself."
        )
    }
}

/// Writes need elevation; `show` does not.
pub fn check_elevated(cmd: &Cmd, elevated: bool) -> Result<(), String> {
    if elevated || *cmd == Cmd::Show {
        Ok(())
    } else {
        Err(needs_elevation(cmd))
    }
}

fn show() -> Result<(), String> {
    let endpoint = Endpoint::default_service();
    let deadline = Instant::now() + Duration::from_millis(800);
    let mut client = Client::connect_deadline(&endpoint, deadline)
        .map_err(|e| format!("the Ferrow service is not reachable at {endpoint}: {e}"))?;
    let info = client.access_info().map_err(|e| {
        format!(
            "the installed service did not answer the access request ({e}); \
             it is probably older than this ferrow. Update it with Ferrow's setup."
        )
    })?;
    print!("{}", format_info(&info));
    Ok(())
}

pub fn run(args: &[OsString]) -> Result<(), String> {
    let cmd = parse(args)?;
    check_elevated(&cmd, ferrowd::accounts::is_elevated())?;
    if cmd == Cmd::Show {
        return show();
    }
    let path = access::default_path().map_err(|e| e.to_string())?;
    let current = access::load_file(&path).unwrap_or_else(|why| {
        eprintln!("ferrow: replacing an unusable access policy ({why})");
        Policy::default()
    });
    let (next, message) = apply(&current, &cmd, &ferrowd::accounts::resolve)?;
    access::write(&path, &next).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("{message}\nThe service applies the change within a second.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn user(id: &str, name: &str) -> AccessUser {
        AccessUser {
            id: id.into(),
            name: name.into(),
        }
    }

    fn lookup(text: &str) -> Result<AccessUser, String> {
        match text {
            "alice" | "1000" => Ok(user("1000", "alice")),
            "bob" | "1001" => Ok(user("1001", "bob")),
            other => Err(format!("no such account: {other}")),
        }
    }

    fn listed(users: &[AccessUser]) -> Policy {
        Policy {
            mode: AccessMode::Listed,
            users: users.to_vec(),
        }
    }

    #[test]
    fn parses_every_subcommand_and_rejects_the_rest() {
        assert_eq!(parse(&os(&[])), Ok(Cmd::Show));
        assert_eq!(parse(&os(&["show"])), Ok(Cmd::Show));
        assert_eq!(parse(&os(&["admins"])), Ok(Cmd::Admins));
        assert_eq!(parse(&os(&["everyone"])), Ok(Cmd::Everyone));
        assert_eq!(
            parse(&os(&["add", "S-1-5-21-1-2-3-1001"])),
            Ok(Cmd::Add("S-1-5-21-1-2-3-1001".into()))
        );
        assert_eq!(
            parse(&os(&["remove", "bob"])),
            Ok(Cmd::Remove("bob".into()))
        );
        for bad in [
            &["add"][..],
            &["remove"],
            &["add", "a", "b"],
            &["all"],
            &["everyone", "x"],
        ] {
            assert!(parse(&os(bad)).unwrap_err().contains("usage"), "{bad:?}");
        }
    }

    #[test]
    fn writes_refuse_without_elevation_and_show_does_not() {
        for cmd in [
            Cmd::Admins,
            Cmd::Everyone,
            Cmd::Add("S-1-5-21-1-2-3-1001".into()),
            Cmd::Remove("bob".into()),
        ] {
            let message = check_elevated(&cmd, false).unwrap_err();
            assert!(message.contains("never elevates itself"), "{message}");
            assert!(message.contains("ferrow service access"));
            assert!(check_elevated(&cmd, true).is_ok());
        }
        assert!(check_elevated(&Cmd::Show, false).is_ok());
        let add = check_elevated(&Cmd::Add("S-1-5-21-9".into()), false).unwrap_err();
        assert!(
            add.contains("add S-1-5-21-9"),
            "the SID is passed through: {add}"
        );
    }

    #[test]
    fn add_lists_the_user_and_switches_to_listed() {
        let (p, message) = apply(&Policy::default(), &Cmd::Add("alice".into()), &lookup).unwrap();
        assert_eq!(p, listed(&[user("1000", "alice")]));
        assert!(message.contains("alice"));
        // Adding the same account twice keeps one entry; another is appended.
        let (p, _) = apply(&p, &Cmd::Add("1000".into()), &lookup).unwrap();
        let (p, _) = apply(&p, &Cmd::Add("bob".into()), &lookup).unwrap();
        assert_eq!(p.users.len(), 2);
        assert_eq!(p.mode, AccessMode::Listed);
    }

    #[test]
    fn add_never_narrows_everyone_and_an_unknown_account_changes_nothing() {
        let everyone = Policy {
            mode: AccessMode::Everyone,
            users: vec![],
        };
        let (p, message) = apply(&everyone, &Cmd::Add("alice".into()), &lookup).unwrap();
        assert_eq!(p, everyone);
        assert!(message.contains("already"));
        assert!(apply(&Policy::default(), &Cmd::Add("mallory".into()), &lookup).is_err());
    }

    #[test]
    fn remove_by_id_name_or_lookup_and_the_last_removal_returns_to_admins() {
        let both = listed(&[user("1000", "alice"), user("1001", "bob")]);
        let (p, _) = apply(&both, &Cmd::Remove("ALICE".into()), &lookup).unwrap();
        assert_eq!(p, listed(&[user("1001", "bob")]));
        let (p, _) = apply(&p, &Cmd::Remove("1001".into()), &lookup).unwrap();
        assert_eq!(p, Policy::default());
        // A listed account that no longer resolves can still be removed by its id.
        let gone = listed(&[user("S-1-5-21-1-2-3-1500", "")]);
        let nothing = |_: &str| Err("gone".to_owned());
        let (p, _) = apply(&gone, &Cmd::Remove("S-1-5-21-1-2-3-1500".into()), &nothing).unwrap();
        assert_eq!(p.mode, AccessMode::Admins);
        assert!(apply(&both, &Cmd::Remove("carol".into()), &lookup).is_err());
        assert!(apply(&Policy::default(), &Cmd::Remove("alice".into()), &lookup).is_err());
    }

    #[test]
    fn admins_and_everyone_clear_the_list() {
        let both = listed(&[user("1000", "alice")]);
        let (admins, _) = apply(&both, &Cmd::Admins, &lookup).unwrap();
        assert_eq!(admins, Policy::default());
        let (everyone, message) = apply(&both, &Cmd::Everyone, &lookup).unwrap();
        assert_eq!(
            (everyone.mode, everyone.users.len()),
            (AccessMode::Everyone, 0)
        );
        assert!(message.contains("every file name"));
    }

    #[test]
    fn add_stops_at_the_user_cap() {
        let full: Vec<AccessUser> = (0..MAX_ACCESS_USERS)
            .map(|n| user(&format!("{}", 2000 + n), ""))
            .collect();
        assert!(apply(&listed(&full), &Cmd::Add("alice".into()), &lookup).is_err());
    }

    #[test]
    fn show_formats_mode_verdict_and_only_the_users_it_was_given() {
        let mut info = AccessInfo {
            mode: AccessMode::Listed,
            you_allowed: false,
            you: user("1002", ""),
            users: None,
        };
        let text = format_info(&info);
        assert!(text.contains("administrators and the listed users"));
        assert!(text.contains("1002 - not answered"));
        assert!(!text.contains("listed user:"));
        info.you_allowed = true;
        info.users = Some(vec![user("1000", "alice")]);
        let text = format_info(&info);
        assert!(text.contains("- answered") && text.contains("listed user: alice (1000)"));
    }
}
