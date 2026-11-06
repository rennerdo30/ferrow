//! Deterministic synthetic index generator.
//!
//! Used by tests (small) and CI benchmarks (5 M entries). The shape mimics
//! a real tree: depth up to about 12, mixed fan-out, names drawn from word
//! lists plus extensions, numbers and dates, with a share of non-ASCII
//! names. Same seed and entry count always produce the same index.

use ferrow_index::{Index, IndexBuilder, flags};

/// Small fast deterministic RNG (xoshiro256** core steps, splitmix seed).
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn new(seed: u64) -> Rng {
        let mut z = seed;
        let mut next = move || {
            z = z.wrapping_add(0x9E3779B97F4A7C15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
            x ^ (x >> 31)
        };
        Rng {
            s: [next(), next(), next(), next()],
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = (self.s[1].wrapping_mul(5)).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform value in `0..bound` (bound must be > 0).
    pub fn below(&mut self, bound: usize) -> usize {
        debug_assert!(bound > 0);
        (self.next_u64() % bound as u64) as usize
    }

    /// True with probability `num / den`.
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.next_u64() % den < num
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const WORDS: &[&str] = &[
    "report",
    "budget",
    "photo",
    "backup",
    "notes",
    "draft",
    "final",
    "project",
    "data",
    "archive",
    "music",
    "video",
    "document",
    "invoice",
    "letter",
    "scan",
    "image",
    "source",
    "build",
    "release",
    "config",
    "settings",
    "readme",
    "license",
    "manual",
    "guide",
    "plan",
    "summary",
    "review",
    "meeting",
    "travel",
    "family",
    "holiday",
    "school",
    "work",
    "home",
    "garden",
    "kitchen",
    "winter",
    "summer",
    "spring",
    "autumn",
    "river",
    "mountain",
    "forest",
    "ocean",
    "desert",
    "island",
    "city",
    "village",
    "bridge",
    "tower",
    "market",
    "station",
    "alpha",
    "beta",
    "gamma",
    "delta",
    "omega",
    "north",
    "south",
    "east",
    "west",
    "central",
    "main",
    "index",
    "log",
    "tmp",
    "temp",
    "cache",
    "lib",
    "src",
    "bin",
    "test",
    "spec",
    "design",
    "model",
    "view",
    "controller",
    "service",
    "client",
    "server",
    "shared",
    "common",
    "util",
    "core",
    "base",
    "app",
    "web",
    "mobile",
    "desktop",
    "cloud",
    "local",
    "remote",
];

const DIR_WORDS: &[&str] = &[
    "Documents",
    "Pictures",
    "Music",
    "Videos",
    "Downloads",
    "Projects",
    "Archive",
    "Backup",
    "Work",
    "Personal",
    "Photos",
    "Scans",
    "Books",
    "Games",
    "Tools",
    "Data",
    "Media",
    "Shared",
    "Incoming",
    "Outgoing",
    "Sorted",
    "Unsorted",
    "Old",
    "New",
    "Misc",
    "Private",
];

const EXTS: &[&str] = &[
    "txt", "md", "pdf", "docx", "xlsx", "pptx", "jpg", "jpeg", "png", "gif", "bmp", "svg", "mp3",
    "flac", "wav", "mp4", "mkv", "mov", "zip", "tar", "gz", "7z", "rs", "toml", "json", "xml",
    "html", "css", "js", "ts", "py", "c", "h", "cpp", "log", "csv", "db", "bin", "dat",
];

const NONASCII_WORDS: &[&str] = &[
    "Bericht",
    "Übung",
    "Ärger",
    "Grüße",
    "Müller",
    "José",
    "niño",
    "résumé",
    "café",
    "naïve",
    "Zürich",
    "München",
    "файл",
    "отчёт",
    "документ",
    "写真",
    "文書",
    "旅行",
    "音楽",
    "αρχείο",
    "Łódź",
    "Český",
    "smör",
    "blåbær",
    "türkçe",
];

const MAX_DEPTH: u32 = 12;

fn file_name(rng: &mut Rng) -> String {
    let mut name = String::new();
    if rng.chance(8, 100) {
        name.push_str(rng.pick(NONASCII_WORDS));
    } else {
        name.push_str(rng.pick(WORDS));
    }
    match rng.below(4) {
        0 => {}
        1 => {
            name.push(' ');
            name.push_str(rng.pick(WORDS));
        }
        2 => {
            name.push('-');
            name.push_str(rng.pick(WORDS));
        }
        _ => {
            name.push('_');
            name.push_str(rng.pick(WORDS));
        }
    }
    match rng.below(4) {
        0 => {}
        1 => name.push_str(&format!(" {}", 2015 + rng.below(11))),
        2 => name.push_str(&format!(" ({})", rng.below(100))),
        _ => name.push_str(&format!("-{:02}", rng.below(50))),
    }
    if rng.chance(90, 100) {
        name.push('.');
        name.push_str(rng.pick(EXTS));
    }
    // A few capitalised names so case handling matters.
    if rng.chance(20, 100) {
        let mut chars = name.chars();
        if let Some(first) = chars.next() {
            name = first.to_uppercase().collect::<String>() + chars.as_str();
        }
    }
    name
}

fn dir_name(rng: &mut Rng) -> String {
    if rng.chance(8, 100) {
        return rng.pick(NONASCII_WORDS).to_string();
    }
    let mut name = rng.pick(DIR_WORDS).to_string();
    if rng.chance(35, 100) {
        name.push(' ');
        name.push_str(&format!("{}", 2015 + rng.below(11)));
    } else if rng.chance(20, 100) {
        name.push('-');
        name.push_str(rng.pick(WORDS));
    }
    name
}

/// Generate an index with exactly `entries` entries (including one root),
/// deterministically from `seed`. `entries` must be at least 1.
pub fn generate(entries: usize, seed: u64) -> Index {
    let builder = IndexBuilder::new();
    generate_into(&builder, entries, seed);
    builder.freeze()
}

/// Fill `builder` with exactly `entries` entries. Exposed so benchmarks
/// can time the freeze step separately from generation.
pub fn generate_into(builder: &IndexBuilder, entries: usize, seed: u64) {
    assert!(entries >= 1, "fixture needs at least the root entry");
    let mut rng = Rng::new(seed);
    let root = builder.add_root(b"fixture-root", flags::DIR);
    if entries == 1 {
        return;
    }
    // Dirs available as parents, with their depth. Recent dirs are picked
    // more often (exponential-ish bias via min of two draws), which gives
    // a realistic mix of wide shallow dirs and deeper chains.
    let mut dirs: Vec<(u32, u32)> = vec![(root, 0)];
    for _ in 1..entries {
        let a = rng.below(dirs.len());
        let b = rng.below(dirs.len());
        let (parent, depth) = dirs[a.max(b)];
        let make_dir = depth < MAX_DEPTH && rng.chance(12, 100);
        if make_dir {
            let id = builder.add(parent, dir_name(&mut rng).as_bytes(), flags::DIR);
            dirs.push((id, depth + 1));
        } else {
            builder.add(parent, file_name(&mut rng).as_bytes(), 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_exact_size() {
        let a = generate(5_000, 42);
        let b = generate(5_000, 42);
        assert_eq!(a.len(), 5_000);
        for id in [0u32, 1, 100, 4_999] {
            assert_eq!(a.name(id), b.name(id));
            assert_eq!(a.parent(id), b.parent(id));
        }
        let c = generate(5_000, 43);
        assert_ne!(a.name(100), c.name(100));
    }

    #[test]
    fn shape_has_depth_and_nonascii() {
        let idx = generate(20_000, 7);
        let mut max_depth = 0u32;
        let mut nonascii = 0usize;
        let mut dirs = 0usize;
        for id in 0..idx.len() as u32 {
            let mut depth = 0u32;
            let mut cur = id;
            while idx.parent(cur) != ferrow_index::ROOT_PARENT {
                depth += 1;
                cur = idx.parent(cur);
            }
            max_depth = max_depth.max(depth);
            if idx.name(id).iter().any(|b| !b.is_ascii()) {
                nonascii += 1;
            }
            if idx.flags(id) & flags::DIR != 0 {
                dirs += 1;
            }
        }
        assert!(max_depth >= 6, "max depth was {max_depth}");
        assert!(nonascii > 100, "non-ASCII count was {nonascii}");
        assert!(dirs > 500, "dir count was {dirs}");
    }

    #[test]
    fn tiny_sizes() {
        assert_eq!(generate(1, 1).len(), 1);
        assert_eq!(generate(2, 1).len(), 2);
    }
}
