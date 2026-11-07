//! Release metadata shared by the app's update check and the elevated
//! updater: version parsing and ordering, the published asset names and
//! the checksum file. No I/O here; everything is unit-tested.

use std::cmp::Ordering;

/// The GitHub repository releases come from. Fixed: neither the app nor
/// the updater accepts a URL from anywhere else.
pub const REPO: &str = "rennerdo30/ferrow";

/// `https://api.github.com/repos/<REPO>/releases/latest`: newest stable.
pub fn latest_api_url() -> String {
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

/// The newest 30 releases, betas included (drafts are never listed for
/// anonymous callers).
pub fn list_api_url() -> String {
    format!("https://api.github.com/repos/{REPO}/releases?per_page=30")
}

/// Download URL of one asset of the release tagged `v<version>`.
pub fn asset_url(version: &Version, asset: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/v{version}/{asset}")
}

/// The human release page.
pub fn release_page(version: &Version) -> String {
    format!("https://github.com/{REPO}/releases/tag/v{version}")
}

/// The Windows setup's asset name. Together with [`SUMS`] this is the
/// contract between the release workflow and the updater.
pub fn setup_name(version: &Version) -> String {
    format!("Ferrow-{version}-setup.exe")
}

/// Checksum file published next to every setup (`sha256sum` format).
pub const SUMS: &str = "SHA256SUMS.txt";

/// `ferrow-update` exit codes; the app turns them into messages.
pub mod exit {
    pub const OK: u8 = 0;
    pub const USAGE: u8 = 2;
    pub const STAGE: u8 = 3;
    pub const DOWNLOAD: u8 = 4;
    pub const CHECKSUM: u8 = 5;
    pub const SETUP_FAILED: u8 = 6;
    pub const SETUP_TIMEOUT: u8 = 7;

    /// What the app shows for an updater exit code.
    pub fn message(code: u32) -> &'static str {
        match code {
            0 => "The update was installed.",
            2 => "The updater was started incorrectly.",
            3 => "The updater could not create its protected download folder.",
            4 => "The update could not be downloaded. Check the network and try again.",
            5 => "The downloaded setup did not match its published checksum, so it was not run.",
            6 => "Setup reported an error. Its log is in the administrator's TEMP folder.",
            7 => "Setup did not finish within 3 minutes.",
            _ => "The updater failed.",
        }
    }
}

/// A semantic version: `MAJOR.MINOR.PATCH[-PRE]`, no build metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Dot-separated pre-release identifiers (`beta.3` → `["beta", "3"]`).
    pub pre: Vec<String>,
}

impl Version {
    /// Parse strictly: no leading zeros, ASCII alphanumerics in the
    /// pre-release, a leading `v` is accepted (tags).
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.strip_prefix('v').unwrap_or(text);
        if text.len() > 64 {
            return None;
        }
        let (core, pre) = match text.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (text, None),
        };
        let mut parts = core.split('.');
        let mut number = || -> Option<u64> {
            let p = parts.next()?;
            if p.is_empty()
                || !p.bytes().all(|b| b.is_ascii_digit())
                || (p.len() > 1 && p.starts_with('0'))
            {
                return None;
            }
            p.parse().ok()
        };
        let (major, minor, patch) = (number()?, number()?, number()?);
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) => {
                let ids: Vec<String> = p.split('.').map(str::to_owned).collect();
                for id in &ids {
                    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
                        return None;
                    }
                    if id.bytes().all(|b| b.is_ascii_digit()) && id.len() > 1 && id.starts_with('0')
                    {
                        return None;
                    }
                }
                ids
            }
        };
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

impl Ord for Version {
    /// SemVer precedence: a release ranks above its pre-releases; numeric
    /// identifiers compare numerically and below alphanumeric ones.
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Which releases an install is offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Published stable releases only.
    Stable,
    /// The newest published release, beta or stable.
    Beta,
}

impl Channel {
    /// Until the user picks one, the channel follows the running version:
    /// a beta build updates on Beta, a stable build on Stable.
    pub fn default_for(current: &Version) -> Channel {
        if current.is_prerelease() {
            Channel::Beta
        } else {
            Channel::Stable
        }
    }
}

/// One published release from the GitHub API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// Whether the release has the Windows setup and the checksum file.
    pub has_setup: bool,
}

/// The release a channel offers from a GitHub API response: the
/// `releases/latest` object (Stable) or the `releases` list (Beta).
/// Drafts and unparseable tags are skipped; Stable never takes a
/// pre-release even if the API marks it latest.
pub fn pick(json: &str, channel: Channel) -> Result<Option<Release>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("unexpected answer from GitHub: {e}"))?;
    let items: Vec<&serde_json::Value> = match &value {
        serde_json::Value::Array(list) => list.iter().collect(),
        object @ serde_json::Value::Object(_) => vec![object],
        _ => return Err("unexpected answer from GitHub".into()),
    };
    let mut best: Option<Release> = None;
    for item in items {
        let flag = |k: &str| item.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        if flag("draft") {
            continue;
        }
        let Some(version) = item
            .get("tag_name")
            .and_then(|t| t.as_str())
            .and_then(Version::parse)
        else {
            continue;
        };
        if channel == Channel::Stable && (flag("prerelease") || version.is_prerelease()) {
            continue;
        }
        let names: Vec<&str> = item
            .get("assets")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("name").and_then(|n| n.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let setup = setup_name(&version);
        let has_setup = names.contains(&setup.as_str()) && names.contains(&SUMS);
        if best.as_ref().is_none_or(|b| version > b.version) {
            best = Some(Release { version, has_setup });
        }
    }
    Ok(best)
}

/// The expected SHA-256 (lowercase hex) of `name` in a `sha256sum` file.
/// Exactly one matching line is required.
pub fn expected_hash(sums: &str, name: &str) -> Result<String, String> {
    let mut found = Vec::new();
    for line in sums.lines() {
        let line = line.trim_end_matches('\r');
        let Some((hash, file)) = line.split_once(' ') else {
            continue;
        };
        let file = file.strip_prefix(['*', ' ']).unwrap_or(file);
        if file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            found.push(hash.to_ascii_lowercase());
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(format!("{SUMS} has no checksum for {name}")),
        _ => Err(format!("{SUMS} lists {name} more than once")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parses_and_prints() {
        assert_eq!(v("v1.2.3").to_string(), "1.2.3");
        assert_eq!(v("0.1.0-beta.12").to_string(), "0.1.0-beta.12");
        for bad in [
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.3-",
            "1.2.3-beta..1",
            "1.2.3-be_ta",
            "1.2.3-beta.01",
            "1.2.3+build",
            "",
            "x.y.z",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn semver_precedence() {
        let order = [
            "0.1.0-alpha",
            "0.1.0-beta.2",
            "0.1.0-beta.10",
            "0.1.0-beta.10.1",
            "0.1.0-rc.1",
            "0.1.0",
            "0.1.1",
            "0.2.0",
            "1.0.0",
        ];
        for pair in order.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(Channel::default_for(&v("0.1.0-beta.1")), Channel::Beta);
        assert_eq!(Channel::default_for(&v("0.1.0")), Channel::Stable);
    }

    fn release(tag: &str, pre: bool, draft: bool) -> String {
        let ver = tag.trim_start_matches('v');
        format!(
            r#"{{"tag_name":"{tag}","prerelease":{pre},"draft":{draft},"assets":[{{"name":"Ferrow-{ver}-setup.exe"}},{{"name":"SHA256SUMS.txt"}}]}}"#
        )
    }

    #[test]
    fn channels_pick_the_right_release() {
        let list = format!(
            "[{},{},{},{}]",
            release("v0.2.0-beta.1", true, false),
            release("v0.1.0", false, false),
            release("v0.3.0", false, true),
            release("not-a-version", false, false),
        );
        let beta = pick(&list, Channel::Beta).unwrap().unwrap();
        assert_eq!(beta.version, v("0.2.0-beta.1"));
        assert!(beta.has_setup);
        let stable = pick(&list, Channel::Stable).unwrap().unwrap();
        assert_eq!(stable.version, v("0.1.0"), "drafts and betas are skipped");
        let latest = release("v0.1.1", false, false);
        assert_eq!(
            pick(&latest, Channel::Stable).unwrap().unwrap().version,
            v("0.1.1")
        );
        assert!(pick("[]", Channel::Beta).unwrap().is_none());
        assert!(pick("{oops", Channel::Beta).is_err());
        let no_assets = r#"{"tag_name":"v0.4.0","prerelease":false,"draft":false,"assets":[]}"#;
        assert!(!pick(no_assets, Channel::Stable).unwrap().unwrap().has_setup);
    }

    #[test]
    fn checksum_file() {
        let h = "a".repeat(64);
        let sums = format!(
            "{h}  Ferrow-0.1.0-setup.exe\r\n{}  other.exe\n",
            "b".repeat(64)
        );
        assert_eq!(expected_hash(&sums, "Ferrow-0.1.0-setup.exe").unwrap(), h);
        assert_eq!(
            expected_hash(&format!("{} *x.exe", "C".repeat(64)), "x.exe").unwrap(),
            "c".repeat(64)
        );
        assert!(expected_hash(&sums, "missing.exe").is_err());
        let twice = format!("{h}  x.exe\n{h}  x.exe\n");
        assert!(expected_hash(&twice, "x.exe").is_err());
        assert!(expected_hash("short  x.exe", "x.exe").is_err());
    }
}
