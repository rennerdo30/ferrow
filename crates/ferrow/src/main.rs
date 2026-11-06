//! Ferrow command-line interface.
//!
//! ```text
//! ferrow index <root>... [--exclude <path>]... [--follow-symlinks] [--out <file>]
//! ferrow search <query> [--index <file>] [--limit N] [--json | --null] [--count]
//! ferrow stats [--index <file>]
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use ferrow_index::Index;
use ferrow_query::{Query, SearchOptions, search};
use ferrow_walk::{WalkOptions, scan};

const USAGE: &str = "\
ferrow - find any file by name, as fast as you type

Usage:
  ferrow index <root>... [--exclude <path>]... [--follow-symlinks] [--out <file>]
  ferrow search <query> [--index <file>] [--limit N] [--json | --null] [--count]
  ferrow stats [--index <file>]
  ferrow --help | --version

Build an index first, then search it:
  ferrow index ~/Documents /data
  ferrow search \"report 2024\"
  ferrow search \"!tmp log\" --limit 20

Query syntax: space = AND, !term = NOT, \"quoted phrase\" is literal,
case:term is case-sensitive. Matching is a case-insensitive substring
test on the file name by default.

The default index file lives in the platform data directory.
";

/// A usable platform base directory from an environment variable:
/// present, non-empty and absolute. Anything else yields `None`, which
/// surfaces as the explicit "pass --index/--out" error instead of a
/// path relative to the working directory.
fn valid_base(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return None;
    }
    Some(path)
}

/// `%LOCALAPPDATA%\ferrow`
fn windows_data_dir(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    Some(valid_base(env("LOCALAPPDATA"))?.join("ferrow"))
}

/// `~/Library/Application Support/ferrow`
fn macos_data_dir(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    Some(
        valid_base(env("HOME"))?
            .join("Library")
            .join("Application Support")
            .join("ferrow"),
    )
}

/// `$XDG_DATA_HOME/ferrow`, falling back to `~/.local/share/ferrow`.
/// Per the XDG base directory spec, a relative `XDG_DATA_HOME` is
/// invalid and ignored.
fn xdg_data_dir(env: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(xdg) = valid_base(env("XDG_DATA_HOME")) {
        return Some(xdg.join("ferrow"));
    }
    Some(
        valid_base(env("HOME"))?
            .join(".local")
            .join("share")
            .join("ferrow"),
    )
}

/// Default index file in the platform per-user data directory.
///
/// Hand-rolled from environment variables instead of a `directories`
/// dependency: that crate's tree pulls in an MPL-2.0 crate, and Ferrow
/// only allows permissive licences (see `deny.toml`).
fn default_index_path() -> Option<PathBuf> {
    let env = |name: &str| std::env::var_os(name);
    let data_dir = if cfg!(windows) {
        windows_data_dir(&env)?
    } else if cfg!(target_os = "macos") {
        macos_data_dir(&env)?
    } else {
        xdg_data_dir(&env)?
    };
    Some(data_dir.join("index.bin"))
}

struct Args {
    positional: Vec<std::ffi::OsString>,
    out: Option<PathBuf>,
    index: Option<PathBuf>,
    excludes: Vec<PathBuf>,
    limit: Option<usize>,
    json: bool,
    null: bool,
    count: bool,
    follow_symlinks: bool,
}

/// Parse arguments keeping paths as `OsString`s, so non-Unicode paths
/// (lone surrogates on Windows, invalid UTF-8 on Unix) work everywhere.
/// Only flag names, `--limit` and the query are treated as text. Flag
/// detection runs on the lossless name bytes (`--flag=value` splitting
/// included); path values round-trip through `osname` unchanged.
fn parse_args(rest: &[std::ffi::OsString]) -> Result<Args, String> {
    use ferrow_index::osname::{bytes_to_os, name_to_bytes};
    let mut args = Args {
        positional: Vec::new(),
        out: None,
        index: None,
        excludes: Vec::new(),
        limit: None,
        json: false,
        null: false,
        count: false,
        follow_symlinks: false,
    };
    // Lossless bytes per token, computed once.
    let tokens: Vec<Vec<u8>> = rest
        .iter()
        .map(|t| {
            let mut b = Vec::new();
            name_to_bytes(t, &mut b);
            b
        })
        .collect();
    let mut i = 0;
    while i < rest.len() {
        let token = &tokens[i];
        // `--flag=value` or `--flag value`
        let (flag, inline): (&[u8], Option<Vec<u8>>) = match token.iter().position(|&b| b == b'=') {
            Some(eq) if token.starts_with(b"--") => (&token[..eq], Some(token[eq + 1..].to_vec())),
            _ => (token.as_slice(), None),
        };
        let flag_text = String::from_utf8_lossy(flag).into_owned();
        let take_value = |i: &mut usize| -> Result<std::ffi::OsString, String> {
            match &inline {
                Some(v) => Ok(bytes_to_os(v)),
                None => {
                    let v = rest
                        .get(*i + 1)
                        .ok_or_else(|| format!("flag {flag_text} needs a value"))?;
                    *i += 1;
                    Ok(v.clone())
                }
            }
        };
        match flag {
            b"--out" => args.out = Some(PathBuf::from(take_value(&mut i)?)),
            b"--index" => args.index = Some(PathBuf::from(take_value(&mut i)?)),
            b"--exclude" => args.excludes.push(PathBuf::from(take_value(&mut i)?)),
            b"--limit" => {
                let v = take_value(&mut i)?;
                let v = v
                    .to_str()
                    .ok_or_else(|| "invalid --limit value: not UTF-8".to_string())?;
                args.limit = Some(
                    v.parse::<usize>()
                        .map_err(|_| format!("invalid --limit value: {v}"))?,
                );
            }
            b"--json" => args.json = true,
            b"--null" | b"-0" => args.null = true,
            b"--count" => args.count = true,
            b"--follow-symlinks" => args.follow_symlinks = true,
            other if other.starts_with(b"--") => {
                return Err(format!("unknown flag: {}", String::from_utf8_lossy(other)));
            }
            _ => args.positional.push(rest[i].clone()),
        }
        i += 1;
    }
    if args.json && args.null {
        return Err("--json and --null cannot be combined".to_string());
    }
    Ok(args)
}

fn index_path(flag: &Option<PathBuf>) -> Result<PathBuf, String> {
    match flag {
        Some(p) => Ok(p.clone()),
        None => default_index_path()
            .ok_or_else(|| "no platform data directory found; pass --index/--out".to_string()),
    }
}

fn cmd_index(args: Args) -> Result<(), String> {
    if args.positional.is_empty() {
        return Err("index needs at least one root path".to_string());
    }
    let out = index_path(&args.out)?;
    let (index, report) = scan(&WalkOptions {
        roots: args
            .positional
            .iter()
            .map(|p| PathBuf::from(p.clone()))
            .collect(),
        excludes: args.excludes,
        follow_symlinks: args.follow_symlinks,
    });
    println!("{}", report.summary());
    for sample in &report.error_samples {
        eprintln!("warning: {sample}");
    }
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    index.save(&out).map_err(|e| e.to_string())?;
    println!("saved {} entries to {}", index.len(), out.display());
    Ok(())
}

fn load(args: &Args) -> Result<(Index, f64), String> {
    let path = index_path(&args.index)?;
    let started = Instant::now();
    let index = Index::load(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((index, started.elapsed().as_secs_f64() * 1000.0))
}

fn json_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

fn cmd_search(args: Args) -> Result<(), String> {
    if args.positional.len() != 1 {
        return Err("search needs exactly one query argument".to_string());
    }
    let query_text = args.positional[0]
        .to_str()
        .ok_or_else(|| "query is not valid UTF-8".to_string())?;
    let query = Query::parse(query_text).map_err(|e| e.to_string())?;
    let (index, _) = load(&args)?;

    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    if args.count {
        let (n, cancelled) = ferrow_query::count(&index, &query, &ferrow_query::CancelToken::new());
        if cancelled {
            return Err("search cancelled".to_string());
        }
        let n = match args.limit {
            Some(limit) => n.min(limit),
            None => n,
        };
        writeln!(out, "{n}").map_err(|e| e.to_string())?;
        return Ok(());
    }

    let result = search(
        &index,
        &query,
        &SearchOptions {
            limit: args.limit,
            ..SearchOptions::default()
        },
    );
    if result.cancelled {
        return Err("search cancelled".to_string());
    }

    let mut buf: Vec<u8> = Vec::new();
    if args.json {
        let mut text = String::from("[");
        for (i, &id) in result.hits.iter().enumerate() {
            index.path_bytes(id, &mut buf);
            if i > 0 {
                text.push(',');
            }
            text.push('"');
            json_escape(&String::from_utf8_lossy(&buf), &mut text);
            text.push('"');
        }
        text.push_str("]\n");
        out.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    } else if args.null {
        for &id in &result.hits {
            index.path_bytes(id, &mut buf);
            out.write_all(&buf).map_err(|e| e.to_string())?;
            out.write_all(&[0]).map_err(|e| e.to_string())?;
        }
    } else {
        for &id in &result.hits {
            index.path_bytes(id, &mut buf);
            out.write_all(&buf).map_err(|e| e.to_string())?;
            out.write_all(b"\n").map_err(|e| e.to_string())?;
        }
    }
    out.flush().map_err(|e| e.to_string())
}

fn cmd_stats(args: Args) -> Result<(), String> {
    let (index, load_ms) = load(&args)?;
    let heap = index.heap_bytes();
    let per_entry = if index.is_empty() {
        0.0
    } else {
        heap as f64 / index.len() as f64
    };
    println!("entries: {}", index.len());
    println!("heap bytes: {heap} ({per_entry:.1} bytes/entry)");
    println!("load time: {load_ms:.1} ms");
    Ok(())
}

fn run(argv: Vec<std::ffi::OsString>) -> Result<(), String> {
    let Some(cmd) = argv.first() else {
        return Err("missing subcommand\n\n".to_string() + USAGE);
    };
    let cmd = cmd.to_str().unwrap_or("");
    match cmd {
        "--help" | "-h" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        "--version" | "-V" => {
            println!("ferrow {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "index" => cmd_index(parse_args(&argv[1..])?),
        "search" => cmd_search(parse_args(&argv[1..])?),
        "stats" => cmd_stats(parse_args(&argv[1..])?),
        other => Err(format!("unknown subcommand: {other}\n\n{USAGE}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + use<> {
        let map: std::collections::HashMap<String, std::ffi::OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), std::ffi::OsString::from(v)))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn abs(parts: &[&str]) -> PathBuf {
        // Platform-absolute base for tests.
        let mut p = if cfg!(windows) {
            PathBuf::from("E:\\base")
        } else {
            PathBuf::from("/base")
        };
        for part in parts {
            p.push(part);
        }
        p
    }

    fn abs_str(parts: &[&str]) -> String {
        abs(parts).to_string_lossy().into_owned()
    }

    #[test]
    fn relative_xdg_data_home_uses_home_fallback() {
        let env = env_of(&[
            ("XDG_DATA_HOME", "relative/data"),
            ("HOME", &abs_str(&["home"])),
        ]);
        assert_eq!(
            xdg_data_dir(&env),
            Some(abs(&["home", ".local", "share", "ferrow"]))
        );
        // An absolute XDG_DATA_HOME wins.
        let env = env_of(&[
            ("XDG_DATA_HOME", &abs_str(&["xdg"])),
            ("HOME", &abs_str(&["home"])),
        ]);
        assert_eq!(xdg_data_dir(&env), Some(abs(&["xdg", "ferrow"])));
    }

    #[test]
    fn empty_platform_base_does_not_write_relative_to_cwd() {
        // Empty or relative bases never produce a (relative) path.
        assert_eq!(windows_data_dir(&env_of(&[("LOCALAPPDATA", "")])), None);
        assert_eq!(
            windows_data_dir(&env_of(&[("LOCALAPPDATA", "relative\\AppData")])),
            None
        );
        assert_eq!(windows_data_dir(&env_of(&[])), None);
        assert_eq!(macos_data_dir(&env_of(&[("HOME", "")])), None);
        assert_eq!(macos_data_dir(&env_of(&[("HOME", "relative/home")])), None);
        assert_eq!(
            xdg_data_dir(&env_of(&[("XDG_DATA_HOME", ""), ("HOME", "")])),
            None
        );
        assert_eq!(xdg_data_dir(&env_of(&[("HOME", "relative/home")])), None);
        // Valid absolute bases work.
        assert_eq!(
            windows_data_dir(&env_of(&[("LOCALAPPDATA", &abs_str(&["Local"]))])),
            Some(abs(&["Local", "ferrow"]))
        );
        assert!(default_index_path().is_none_or(|p| p.is_absolute()));
    }
}

fn main() -> ExitCode {
    // args_os: paths may be non-Unicode; std::env::args() would panic.
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    match run(argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}
