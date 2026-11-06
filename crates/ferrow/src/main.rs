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

/// Default index file in the platform per-user data directory.
///
/// Hand-rolled from environment variables instead of a `directories`
/// dependency: that crate's tree pulls in an MPL-2.0 crate, and Ferrow
/// only allows permissive licences (see `deny.toml`).
fn default_index_path() -> Option<PathBuf> {
    let data_dir: PathBuf = if cfg!(windows) {
        // %LOCALAPPDATA%\ferrow
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("ferrow")
    } else if cfg!(target_os = "macos") {
        // ~/Library/Application Support/ferrow
        PathBuf::from(std::env::var_os("HOME")?)
            .join("Library")
            .join("Application Support")
            .join("ferrow")
    } else {
        // $XDG_DATA_HOME/ferrow or ~/.local/share/ferrow
        match std::env::var_os("XDG_DATA_HOME") {
            Some(x) if !x.is_empty() => PathBuf::from(x).join("ferrow"),
            _ => PathBuf::from(std::env::var_os("HOME")?)
                .join(".local")
                .join("share")
                .join("ferrow"),
        }
    };
    Some(data_dir.join("index.bin"))
}

struct Args {
    positional: Vec<String>,
    out: Option<PathBuf>,
    index: Option<PathBuf>,
    excludes: Vec<PathBuf>,
    limit: Option<usize>,
    json: bool,
    null: bool,
    count: bool,
    follow_symlinks: bool,
}

fn parse_args(rest: &[String]) -> Result<Args, String> {
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
    let mut i = 0;
    let need = |i: &usize, flag: &str, rest: &[String]| -> Result<String, String> {
        rest.get(i + 1)
            .cloned()
            .ok_or_else(|| format!("flag {flag} needs a value"))
    };
    while i < rest.len() {
        // `--flag=value` or `--flag value`
        let (flag, inline): (&str, Option<String>) = match rest[i].split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (rest[i].as_str(), None),
        };
        let value = |i: &mut usize| -> Result<String, String> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => {
                    let v = need(i, flag, rest)?;
                    *i += 1;
                    Ok(v)
                }
            }
        };
        match flag {
            "--out" => args.out = Some(PathBuf::from(value(&mut i)?)),
            "--index" => args.index = Some(PathBuf::from(value(&mut i)?)),
            "--exclude" => args.excludes.push(PathBuf::from(value(&mut i)?)),
            "--limit" => {
                let v = value(&mut i)?;
                args.limit = Some(
                    v.parse::<usize>()
                        .map_err(|_| format!("invalid --limit value: {v}"))?,
                );
            }
            "--json" => args.json = true,
            "--null" | "-0" => args.null = true,
            "--count" => args.count = true,
            "--follow-symlinks" => args.follow_symlinks = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown flag: {other}"));
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
        roots: args.positional.iter().map(PathBuf::from).collect(),
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
    let query = Query::parse(&args.positional[0]).map_err(|e| e.to_string())?;
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

fn run(argv: Vec<String>) -> Result<(), String> {
    let Some(cmd) = argv.first() else {
        return Err("missing subcommand\n\n".to_string() + USAGE);
    };
    match cmd.as_str() {
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

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match run(argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}
