//! Fitting text into a column without losing what matters: paths get a
//! middle ellipsis (the drive and top folder, then the last folders), names
//! keep their extension. Pure functions over a `measure` callback (the width
//! of a string), so they are tested without a font.

const ELLIPSIS: &str = "…";

/// Width of a string in the unit of the `max` the caller fits it into.
pub type Measure<'a> = &'a dyn Fn(&str) -> f32;

/// A path split into its root and folder names.
struct Split<'a> {
    /// `C:\`, `\\server\share\`, `/` or empty (relative).
    root: &'a str,
    segments: Vec<&'a str>,
    sep: char,
}

fn split_path(path: &str) -> Split<'_> {
    let sep = if path.contains('\\') { '\\' } else { '/' };
    let b = path.as_bytes();
    let root_len = if path.starts_with("\\\\") {
        // UNC: `\\server\share\`.
        let mut seps = 0;
        let mut end = path.len();
        for (i, c) in path.char_indices().skip(2) {
            if c == '\\' {
                seps += 1;
                if seps == 2 {
                    end = i + 1;
                    break;
                }
            }
        }
        end
    } else if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        if b.len() >= 3 && (b[2] == b'\\' || b[2] == b'/') {
            3
        } else {
            2
        }
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let (root, rest) = path.split_at(root_len);
    Split {
        root,
        segments: rest.split(sep).filter(|s| !s.is_empty()).collect(),
        sep,
    }
}

/// `path` shortened to fit `max`, fitted from the END, because the end of a
/// path is what tells folders apart (`…\宇多田ヒカル\Album 1` against
/// `…\Daft Punk\Album 1`):
///
/// 1. As many trailing folders as fit, behind `…`, with the root (`C:\`,
///    `\\server\share\`, `/`) in front, but only when there is room for the
///    root, `…` and at least two trailing folders.
/// 2. Otherwise the root goes first, never the second-to-last folder:
///    `…\folder\last`, down to `…\last`.
/// 3. A last folder that is too long on its own keeps its end:
///    `…ting Documents`.
pub fn fit_path(path: &str, max: f32, measure: Measure<'_>) -> String {
    if measure(path) <= max {
        return path.to_owned();
    }
    let Split {
        root,
        segments,
        sep,
    } = split_path(path);
    let n = segments.len();
    if n == 0 {
        return path.to_owned();
    }
    let sep_text = sep.to_string();
    let build = |with_root: bool, k: usize| {
        let mut s = String::new();
        if with_root {
            s.push_str(root);
        }
        s.push_str(ELLIPSIS);
        s.push(sep);
        s.push_str(&segments[n - k..].join(&sep_text));
        s
    };
    // The most trailing folders (`lo..=hi`) a candidate can keep and still
    // fit: more folders is wider, so bisect.
    let most = |with_root: bool, lo: usize, hi: usize| {
        let (mut lo, mut hi, mut best) = (lo, hi, None);
        while lo <= hi {
            let mid = (lo + hi) / 2;
            if measure(&build(with_root, mid)) <= max {
                best = Some(mid);
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        best
    };
    if !root.is_empty()
        && n >= 3
        && let Some(k) = most(true, 2, n - 1)
    {
        return build(true, k);
    }
    if let Some(k) = most(false, 1, n) {
        return build(false, k);
    }
    // Even `…\last` is too wide: keep the end of the last folder.
    let last = segments[n - 1];
    let ends: Vec<usize> = last.char_indices().map(|(i, _)| i).collect();
    let (mut lo, mut hi, mut best) = (1usize, ends.len(), 1usize);
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let cut = format!("{ELLIPSIS}{}", &last[ends[ends.len() - mid]..]);
        if measure(&cut) <= max {
            best = mid;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    format!("{ELLIPSIS}{}", &last[ends[ends.len() - best]..])
}

/// A name cut in the middle: `name[..head] + … + name[tail..]`.
#[derive(Debug, PartialEq, Eq)]
pub struct FittedName {
    pub text: String,
    /// Bytes of the original kept at the start.
    pub head: usize,
    /// Byte offset of the original where the kept end begins.
    pub tail: usize,
}

/// Byte offset where the extension (`.pdf`) starts, or the name's length.
fn extension_start(name: &str) -> usize {
    match name.rfind('.') {
        Some(at) if at > 0 && name.len() - at <= 13 => at,
        _ => name.len(),
    }
}

/// `name` cut in the middle so its extension (and a few characters before
/// it) stay visible: `Annual report - draft…(final).pdf`. `None` when it
/// already fits.
pub fn fit_name(name: &str, max: f32, measure: Measure<'_>) -> Option<FittedName> {
    if measure(name) <= max {
        return None;
    }
    let ext = extension_start(name);
    // Characters of the stem kept next to the extension, widest first.
    for extra in [8usize, 4, 0] {
        let tail = name[..ext]
            .char_indices()
            .rev()
            .nth(extra.saturating_sub(1))
            .filter(|_| extra > 0)
            .map_or(ext, |(i, _)| i);
        // End byte of each head length in characters (0 characters = 0).
        let ends: Vec<usize> = std::iter::once(0)
            .chain(name[..tail].char_indices().map(|(i, c)| i + c.len_utf8()))
            .collect();
        let build = |chars: usize| FittedName {
            text: format!("{}{ELLIPSIS}{}", &name[..ends[chars]], &name[tail..]),
            head: ends[chars],
            tail,
        };
        // The longest head (in characters) that fits.
        let (mut lo, mut hi) = (0usize, ends.len() - 1);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if measure(&build(mid).text) <= max {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        if lo >= 4 || extra == 0 {
            return Some(build(lo));
        }
    }
    unreachable!("the last candidate (extra = 0) always returns")
}

/// Byte ranges of `name` (highlight ranges) mapped into the cut name text.
pub fn remap_ranges(
    ranges: &[std::ops::Range<usize>],
    name_len: usize,
    fitted: &FittedName,
) -> Vec<std::ops::Range<usize>> {
    let ellipsis = ELLIPSIS.len();
    let mut out = Vec::new();
    for r in ranges {
        let (s, e) = (r.start.min(name_len), r.end.min(name_len));
        // Part inside the kept head.
        let (hs, he) = (s.min(fitted.head), e.min(fitted.head));
        if hs < he {
            out.push(hs..he);
        }
        // Part inside the kept tail.
        let (ts, te) = (s.max(fitted.tail), e.max(fitted.tail));
        if ts < te {
            let shift = fitted.head + ellipsis;
            out.push(ts - fitted.tail + shift..te - fitted.tail + shift);
        }
    }
    out
}

/// The `name:value` filters in a search text, in order (`ext:pdf`,
/// `dm:today`), for the "no results" message. Quoted text is skipped.
pub fn query_filters(query: &str) -> Vec<String> {
    const FILTERS: [&str; 8] = [
        "ext:", "parent:", "depth:", "size:", "dm:", "dc:", "da:", "attrib:",
    ];
    let mut out = Vec::new();
    let mut in_quote = false;
    for token in query.split_whitespace() {
        let was_quoted = in_quote;
        if token.matches('"').count() % 2 == 1 {
            in_quote = !in_quote;
        }
        if was_quoted || token.starts_with('"') {
            continue;
        }
        let bare = token.trim_start_matches(['!', '<', '(']);
        let bare = bare.trim_end_matches(['>', ')', '|']);
        if FILTERS
            .iter()
            .any(|f| bare.starts_with(f) && bare.len() > f.len())
        {
            out.push(bare.to_owned());
        }
    }
    out
}

/// A count for the status bar: exact up to 99,999, then `345 K` and `1.2 M`.
pub fn compact_count(n: u64) -> String {
    let group = |n: u64| {
        let s = n.to_string();
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(c);
        }
        out
    };
    match n {
        0..=99_999 => group(n),
        100_000..=999_999 => format!("{} K", n / 1000),
        _ => format!("{:.1} M", n as f64 / 1_000_000.0),
    }
}

/// Cut folder and name texts of the rows on screen, kept while the rows and
/// column widths stay the same, so a repaint (a scroll step, a hover) only
/// looks them up instead of measuring text again.
#[derive(Default)]
pub struct FitCache {
    key: (u64, u32, u32),
    folders: std::collections::HashMap<u32, std::sync::Arc<str>>,
    names: std::collections::HashMap<u32, Option<std::sync::Arc<FittedName>>>,
}

/// More entries than any window shows; reaching it means the key never
/// changed while the user scrolled a long way, so start over.
const CACHE_LIMIT: usize = 4096;

impl FitCache {
    /// Start a frame: the cache is emptied when the rows (`epoch`) or the
    /// column widths changed.
    pub fn begin(&mut self, epoch: u64, folder_width: f32, name_width: f32) {
        let key = (epoch, folder_width as u32, name_width as u32);
        if key != self.key || self.folders.len() + self.names.len() > CACHE_LIMIT {
            self.key = key;
            self.folders.clear();
            self.names.clear();
        }
    }

    /// The cut text of folder entry `folder`, made by `make` on a miss.
    pub fn folder(&mut self, folder: u32, make: impl FnOnce() -> String) -> std::sync::Arc<str> {
        self.folders
            .entry(folder)
            .or_insert_with(|| make().into())
            .clone()
    }

    /// The cut name of entry `id` (`None`: it fits), made by `make` on a miss.
    pub fn name(
        &mut self,
        id: u32,
        make: impl FnOnce() -> Option<FittedName>,
    ) -> Option<std::sync::Arc<FittedName>> {
        self.names
            .entry(id)
            .or_insert_with(|| make().map(std::sync::Arc::new))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Width in "pixels": 1 per character, 2 for wide (CJK) characters.
    fn width(s: &str) -> f32 {
        s.chars()
            .map(|c| if (c as u32) >= 0x2E80 { 2.0 } else { 1.0 })
            .sum()
    }

    fn fit(path: &str, max: f32) -> String {
        fit_path(path, max, &width)
    }

    #[test]
    fn a_path_that_fits_is_unchanged() {
        assert_eq!(
            fit(r"C:\Users\Alex\Documents", 40.0),
            r"C:\Users\Alex\Documents"
        );
    }

    #[test]
    fn long_paths_keep_the_root_and_as_many_last_folders_as_fit() {
        let p = r"D:\Archive\Clients\Northwind Traders\2023\Quarterly reports";
        assert_eq!(
            fit(p, 45.0),
            r"D:\…\Northwind Traders\2023\Quarterly reports"
        );
        assert_eq!(fit(p, 30.0), r"D:\…\2023\Quarterly reports");
    }

    #[test]
    fn the_root_goes_before_the_second_to_last_folder() {
        let p = r"D:\Archive\Clients\Northwind Traders\2023\Quarterly reports";
        // Root + … + two folders needs 27: one short of that, the root goes
        // and the two folders stay.
        assert_eq!(fit(p, 26.0), r"…\2023\Quarterly reports");
        // Then the second-to-last folder goes.
        assert_eq!(fit(p, 20.0), r"…\Quarterly reports");
    }

    #[test]
    fn folders_that_differ_stay_visible_in_a_narrow_column() {
        // The case behind the rule: three rows that differ only in the
        // artist folder must not collapse to the same text.
        let rows = [
            r"C:\Users\Alex\Music\Daft Punk\Album 1",
            r"C:\Users\Alex\Music\宇多田ヒカル\Album 1",
            r"C:\Users\Alex\Music\Khruangbin\Album 1",
        ];
        let cut: Vec<String> = rows.iter().map(|r| fit(r, 24.0)).collect();
        // The root stays on the rows that have room for it; every row keeps
        // its artist folder.
        assert_eq!(
            cut,
            [
                r"C:\…\Daft Punk\Album 1",
                r"…\宇多田ヒカル\Album 1",
                r"C:\…\Khruangbin\Album 1"
            ]
        );
    }

    #[test]
    fn a_whole_drive_root_and_short_paths_are_never_cut() {
        assert_eq!(fit(r"C:\", 3.0), r"C:\");
        assert_eq!(fit(r"C:\Temp", 7.0), r"C:\Temp");
        assert_eq!(fit("", 5.0), "");
    }

    #[test]
    fn unc_paths_keep_server_and_share_while_there_is_room() {
        let p = r"\\nas\media\photos\2026\Tokyo trip\day 3\raw";
        assert_eq!(fit(p, 34.0), r"\\nas\media\…\Tokyo trip\day 3\raw");
        assert_eq!(fit(p, 25.0), r"\\nas\media\…\day 3\raw");
        // No room for the share: the last folders win.
        assert_eq!(fit(p, 20.0), r"…\day 3\raw");
    }

    #[test]
    fn unix_paths_use_their_separator() {
        let p = "/home/alex/projects/ferrow/crates/ferrow-gui/src";
        assert_eq!(fit(p, 24.0), "/…/crates/ferrow-gui/src");
        assert_eq!(fit(p, 16.0), "…/ferrow-gui/src");
    }

    #[test]
    fn japanese_folders_count_by_their_wide_width() {
        let p = r"C:\Users\Alex\Pictures\東京旅行 2026\パノラマ";
        // Wide characters count 2: `パノラマ` is 8, `東京旅行 2026` is 13.
        assert_eq!(fit(p, 28.0), r"C:\…\東京旅行 2026\パノラマ");
        assert_eq!(fit(p, 24.0), r"…\東京旅行 2026\パノラマ");
        assert_eq!(fit(p, 12.0), r"…\パノラマ");
        // Not even `…\last`: the end of the last folder, whole characters.
        assert_eq!(fit(p, 5.0), "…ラマ");
        for max in [5.0, 12.0, 24.0, 28.0, 40.0] {
            assert!(width(&fit(p, max)) <= max);
        }
    }

    #[test]
    fn a_very_long_last_folder_keeps_its_end() {
        let long = "x".repeat(80);
        let expected = format!("…{}", "x".repeat(19));
        assert_eq!(fit(&format!(r"C:\{long}"), 20.0), expected);
        assert_eq!(fit(&format!(r"C:\a\b\{long}"), 20.0), expected);
        let p = r"D:\Archive\Clients\Supporting Documents";
        assert_eq!(fit(p, 15.0), "…ting Documents");
    }

    #[test]
    fn a_name_that_fits_is_none() {
        assert_eq!(fit_name("report.pdf", 12.0, &width), None);
    }

    #[test]
    fn a_cut_name_keeps_its_extension_and_the_end_of_the_stem() {
        let name = "Annual report - draft for review (final).pdf";
        let cut = fit_name(name, 34.0, &width).unwrap();
        assert!(width(&cut.text) <= 34.0, "{}", cut.text);
        assert!(cut.text.ends_with("(final).pdf"), "{}", cut.text);
        assert!(cut.text.starts_with("Annual report"), "{}", cut.text);
        assert!(cut.text.contains('…'));
        // Narrower: the end of the stem shrinks, the extension stays.
        let cut = fit_name(name, 18.0, &width).unwrap();
        assert!(width(&cut.text) <= 18.0, "{}", cut.text);
        assert!(cut.text.ends_with(".pdf"), "{}", cut.text);
        assert!(cut.text.starts_with("Annu"), "{}", cut.text);
    }

    #[test]
    fn a_name_without_extension_keeps_its_end() {
        let cut = fit_name("a-very-long-name-without-any-extension", 20.0, &width).unwrap();
        assert!(width(&cut.text) <= 20.0);
        assert!(cut.text.ends_with("tension"), "{}", cut.text);
    }

    #[test]
    fn cutting_never_splits_a_japanese_character() {
        let name = "営業会議 議事録 2026-10 最終版 確認用.docx";
        for max in [12.0, 20.0, 26.0, 33.0] {
            let cut = fit_name(name, max, &width).unwrap();
            assert!(width(&cut.text) <= max, "{} > {max}", cut.text);
            assert!(cut.text.ends_with(".docx"));
            assert_eq!(
                cut.text,
                format!("{}…{}", &name[..cut.head], &name[cut.tail..])
            );
        }
    }

    #[test]
    fn highlights_follow_the_cut_text() {
        let name = "Annual report - draft for review (final).pdf";
        let cut = fit_name(name, 34.0, &width).unwrap();
        // "report" is in the kept head, "final" in the kept tail.
        let at = |w: &str| name.find(w).unwrap();
        let ranges = vec![at("report")..at("report") + 6, at("final")..at("final") + 5];
        let mapped = remap_ranges(&ranges, name.len(), &cut);
        let words: Vec<&str> = mapped.iter().map(|r| &cut.text[r.clone()]).collect();
        assert_eq!(words, ["report", "final"]);
        // A range inside the removed middle disappears.
        let review = at("review")..at("review") + 6;
        let gone = remap_ranges(std::slice::from_ref(&review), name.len(), &cut);
        assert!(gone.is_empty());
    }

    #[test]
    fn filters_are_listed_in_order() {
        assert_eq!(
            query_filters("zzqx quarterly ext:pdf dm:today"),
            ["ext:pdf", "dm:today"]
        );
        assert_eq!(
            query_filters("<ext:pdf | size:>10mb> !tmp"),
            ["ext:pdf", "size:>10mb"]
        );
        assert!(query_filters("report 2024").is_empty());
        // A bare prefix, or text inside quotes, is not a filter.
        assert!(query_filters("ext: \"say dm:today now\"").is_empty());
    }

    #[test]
    fn counts_are_compact() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(1_694), "1,694");
        assert_eq!(compact_count(99_999), "99,999");
        assert_eq!(compact_count(345_678), "345 K");
        assert_eq!(compact_count(1_204_331), "1.2 M");
        assert_eq!(compact_count(18_422_907), "18.4 M");
    }

    #[test]
    fn the_cache_measures_each_row_once_until_the_key_changes() {
        let mut cache = FitCache::default();
        let mut calls = 0;
        cache.begin(1, 300.0, 200.0);
        for _ in 0..3 {
            cache.folder(7, || {
                calls += 1;
                "x".into()
            });
        }
        assert_eq!(calls, 1);
        // Same frame key: still cached. A new width: measured again.
        cache.begin(1, 300.0, 200.0);
        cache.folder(7, || unreachable!("cached"));
        cache.begin(1, 310.0, 200.0);
        cache.folder(7, || {
            calls += 1;
            "y".into()
        });
        assert_eq!(calls, 2);
        // New rows (epoch): measured again, and `None` ("fits") is cached too.
        cache.begin(2, 310.0, 200.0);
        let mut name_calls = 0;
        for _ in 0..2 {
            assert!(
                cache
                    .name(3, || {
                        name_calls += 1;
                        None
                    })
                    .is_none()
            );
        }
        assert_eq!(name_calls, 1);
    }
}
