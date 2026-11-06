//! Ferrow command-line interface.
//!
//! ```text
//! ferrow index <root>... [--exclude <path>]... [--follow-symlinks] [--out <file>]
//! ferrow search <query> [--index <file>] [--limit N] [--json | --null] [--count]
//! ferrow stats [--index <file>]
//! ferrow status
//! ferrow service install | uninstall | status | run
//! ```
//!
//! `search` talks to the `ferrowd` service when one is reachable
//! (unless `--index` pins an index file) and otherwise falls back
//! to the in-process index (M2b). `status` says which mode is
//! active and why.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use ferrow_index::{ColumnSet, EntryMetadata, Index};
use ferrow_ipc::{Client, Endpoint, Stream};
use ferrow_query::{Query, SearchOptions, SortKey, search};
use ferrow_walk::{WalkOptions, scan};

const USAGE: &str = "\
ferrow - find any file by name, as fast as you type

Usage:
  ferrow index <root>... [--exclude <path>]... [--follow-symlinks] [--out <file>]
  ferrow search <query> [--index <file>] [--limit N] [--json | --null] [--count]
  Search options: --sort name|size|modified|created|accessed --desc
                  --columns path,size,modified,created,accessed,attributes
  Index options: --index-columns none|size,modified,created,accessed,attributes
  Times in output are signed 100 ns ticks since 1970-01-01 UTC.
  ferrow stats [--index <file>]
  ferrow status
  ferrow service install | uninstall | status
  ferrow service run [<root>...] [--index <file>] [--exclude <path>]... [--follow-symlinks]
  ferrow --help | --version

Search uses the ferrowd service when it is reachable and falls
back to the saved index otherwise; `ferrow status` says which.
`service run` starts the daemon unprivileged in the foreground
(per-user endpoint) for development.

Build an index first, then search it:
  ferrow index ~/Documents /data
  ferrow search \"report 2024\"
  ferrow search \"!tmp log\" --limit 20

Query syntax: space = AND, | = OR, ! = NOT, <...> groups terms.
\"quoted phrase\" is literal. Plain terms are case-insensitive substrings.
Wildcards match the whole name: * = any run, ? = one Unicode scalar.
Modifiers stack: case:, path:, wholeword:, file:, folder:, regex:.
Filters: ext:txt;md, parent:\"/folder\", depth:2 (root depth is zero).
Size, date and attribute filters are reserved for a later milestone.
Quote the query for your shell, for example:
  ferrow search \"*.log\"
  ferrow search \"<report*|draft> !tmp\"

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
    sort: SortKey,
    descending: bool,
    columns: Vec<String>,
    index_columns: ColumnSet,
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
        sort: SortKey::Name,
        descending: false,
        columns: vec!["path".into()],
        index_columns: ColumnSet::DEFAULT,
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
            b"--sort" => {
                args.sort = match take_value(&mut i)?.to_str() {
                    Some("name") => SortKey::Name,
                    Some("size") => SortKey::Size,
                    Some("modified") => SortKey::Modified,
                    Some("created") => SortKey::Created,
                    Some("accessed") => SortKey::Accessed,
                    _ => return Err("--sort needs name|size|modified|created|accessed".into()),
                };
            }
            b"--desc" => args.descending = true,
            b"--columns" => {
                args.columns = take_value(&mut i)?
                    .to_str()
                    .ok_or("--columns needs UTF-8")?
                    .split(',')
                    .map(str::to_owned)
                    .collect();
                if args.columns.iter().any(|c| {
                    !matches!(
                        c.as_str(),
                        "path" | "size" | "modified" | "created" | "accessed" | "attributes"
                    )
                }) {
                    return Err("unknown output column".into());
                }
            }
            b"--index-columns" => {
                let text = take_value(&mut i)?;
                args.index_columns = ColumnSet::NONE;
                for column in text
                    .to_str()
                    .ok_or("--index-columns needs UTF-8")?
                    .split(',')
                {
                    args.index_columns = args.index_columns.union(match column {
                        "none" => ColumnSet::NONE,
                        "size" => ColumnSet::SIZE,
                        "modified" => ColumnSet::MODIFIED,
                        "created" => ColumnSet::CREATED,
                        "accessed" => ColumnSet::ACCESSED,
                        "attributes" => ColumnSet::ATTRIBUTES,
                        _ => return Err("unknown index column".into()),
                    });
                }
            }
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

/// Create `dir` and any missing parents. On Unix, directories created
/// by this call get mode 0700 (existing directories keep their mode),
/// so the per-user app data directory is private from the start.
fn create_private_dir_all(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        // Collect the missing chain, deepest first.
        let mut missing: Vec<PathBuf> = Vec::new();
        let mut current = Some(dir.to_path_buf());
        while let Some(path) = current {
            if path.exists() {
                break;
            }
            current = path.parent().map(|p| p.to_path_buf());
            missing.push(path);
        }
        for path in missing.iter().rev() {
            std::fs::DirBuilder::new().mode(0o700).create(path)?;
            // DirBuilder mode is still masked by umask; pin it exactly.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

fn cmd_index(args: Args) -> Result<(), String> {
    if args.positional.is_empty() {
        return Err("index needs at least one root path".to_string());
    }
    let out = index_path(&args.out)?;
    let (index, report) = scan(&WalkOptions {
        columns: args.index_columns,
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
    // Fail closed (N6): an unresolvable existing exclude aborted the
    // scan. Write nothing; an existing index file stays unchanged.
    if let Some(fatal) = report.fatal_error {
        return Err(fatal);
    }
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        // The default app data directory is created privately (0700 on
        // Unix); an explicit --out parent keeps default permissions.
        let created = if args.out.is_none() {
            create_private_dir_all(parent)
        } else {
            std::fs::create_dir_all(parent)
        };
        created.map_err(|e| format!("creating {}: {e}", parent.display()))?;
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

/// Service endpoints to try, in order. `FERROW_ENDPOINT_NAME`
/// (a short pipe/socket name) pins one endpoint, for tests and
/// development daemons; otherwise the system service first,
/// then a per-user `ferrow service run` daemon.
fn service_candidates() -> Vec<Endpoint> {
    if let Ok(name) = std::env::var("FERROW_ENDPOINT_NAME")
        && !name.is_empty()
    {
        return vec![Endpoint::named(&name)];
    }
    vec![Endpoint::default_service(), Endpoint::user_service()]
}

struct DiscoveryFailure {
    endpoint: Endpoint,
    error: std::io::Error,
}

struct Discovery {
    connected: Option<(Client<Stream>, Endpoint)>,
    failures: Vec<DiscoveryFailure>,
}

fn service_client() -> Discovery {
    let mut failures = Vec::new();
    for endpoint in service_candidates() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let result = (|| {
            let mut client = Client::connect_deadline(&endpoint, deadline)?;
            client.status()?; // includes the interim visibility authorization check
            client.clear_deadline();
            Ok::<_, std::io::Error>(client)
        })();
        match result {
            Ok(client) => {
                return Discovery {
                    connected: Some((client, endpoint)),
                    failures,
                };
            }
            Err(error) => failures.push(DiscoveryFailure { endpoint, error }),
        }
    }
    Discovery {
        connected: None,
        failures,
    }
}

/// The formatter retains only the largest escaped path, in every mode.
struct PathOutput<'a, W: Write> {
    out: &'a mut W,
    json: bool,
    null: bool,
    first: bool,
    escaped: String,
}

impl<'a, W: Write> PathOutput<'a, W> {
    fn row(
        &mut self,
        path: &[u8],
        metadata: EntryMetadata,
        columns: &[String],
    ) -> Result<(), String> {
        fn value(
            out: &mut impl Write,
            value: Option<impl std::fmt::Display>,
            json: bool,
        ) -> std::io::Result<()> {
            match value {
                Some(v) => write!(out, "{v}"),
                None => out.write_all(if json { b"null" } else { b"-" }),
            }
        }
        if self.json {
            self.escaped.clear();
            if !self.first {
                self.escaped.push(',');
            }
            self.escaped.push_str("{\"path\":\"");
            json_escape(&String::from_utf8_lossy(path), &mut self.escaped);
            self.escaped.push_str("\",\"size\":");
            self.out
                .write_all(self.escaped.as_bytes())
                .map_err(|e| e.to_string())?;
            value(self.out, metadata.size, true).map_err(|e| e.to_string())?;
            for (name, v) in [
                ("modified", metadata.modified),
                ("created", metadata.created),
                ("accessed", metadata.accessed),
            ] {
                write!(self.out, ",\"{name}\":").map_err(|e| e.to_string())?;
                value(self.out, v, true).map_err(|e| e.to_string())?;
            }
            self.out
                .write_all(b",\"attributes\":")
                .map_err(|e| e.to_string())?;
            value(self.out, metadata.attributes, true).map_err(|e| e.to_string())?;
            self.out.write_all(b"}").map_err(|e| e.to_string())?;
        } else if self.null || (columns.len() == 1 && columns[0] == "path") {
            return self.path(path);
        } else {
            for (i, column) in columns.iter().enumerate() {
                if i > 0 {
                    self.out.write_all(b"\t").map_err(|e| e.to_string())?;
                }
                match column.as_str() {
                    "path" => self.out.write_all(path),
                    "size" => value(self.out, metadata.size, false),
                    "modified" => value(self.out, metadata.modified, false),
                    "created" => value(self.out, metadata.created, false),
                    "accessed" => value(self.out, metadata.accessed, false),
                    "attributes" => value(self.out, metadata.attributes, false),
                    _ => unreachable!("validated columns"),
                }
                .map_err(|e| e.to_string())?;
            }
            self.out.write_all(b"\n").map_err(|e| e.to_string())?;
        }
        self.first = false;
        Ok(())
    }
    fn new(out: &'a mut W, json: bool, null: bool) -> Result<Self, String> {
        if json {
            out.write_all(b"[").map_err(|e| e.to_string())?;
        }
        Ok(Self {
            out,
            json,
            null,
            first: true,
            escaped: String::new(),
        })
    }
    fn path(&mut self, path: &[u8]) -> Result<(), String> {
        if self.json {
            self.escaped.clear();
            if !self.first {
                self.escaped.push(',');
            }
            self.escaped.push('"');
            json_escape(&String::from_utf8_lossy(path), &mut self.escaped);
            self.escaped.push('"');
            self.out
                .write_all(self.escaped.as_bytes())
                .map_err(|e| e.to_string())?;
        } else {
            self.out.write_all(path).map_err(|e| e.to_string())?;
            self.out
                .write_all(if self.null { &[0] } else { b"\n" })
                .map_err(|e| e.to_string())?;
        }
        self.first = false;
        Ok(())
    }
    fn finish(self) -> Result<(), String> {
        if self.json {
            self.out.write_all(b"]\n").map_err(|e| e.to_string())?;
        }
        self.out.flush().map_err(|e| e.to_string())
    }
}

fn emit_paths(
    out: &mut impl Write,
    paths: &[Vec<u8>],
    json: bool,
    null: bool,
) -> Result<(), String> {
    let mut output = PathOutput::new(out, json, null)?;
    for path in paths {
        output.path(path)?;
    }
    output.finish()
}

fn emit_hits<W: Write>(
    out: &mut W,
    index: &Index,
    hits: impl IntoIterator<Item = u32>,
    json: bool,
    null: bool,
    columns: &[String],
) -> Result<(), String> {
    let mut output = PathOutput::new(out, json, null)?;
    let mut buf = Vec::new();
    for id in hits {
        index.path_bytes(id, &mut buf);
        output.row(&buf, index.metadata(id), columns)?;
    }
    output.finish()
}

fn cmd_search(args: Args) -> Result<(), String> {
    if args.positional.len() != 1 {
        return Err("search needs exactly one query argument".to_string());
    }
    let query_text = args.positional[0]
        .to_str()
        .ok_or_else(|| "query is not valid UTF-8".to_string())?;
    let query = Query::parse(query_text).map_err(|e| e.to_string())?;

    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    // Zero is an explicit empty result, while protocol zero means unlimited.
    if args.limit == Some(0) {
        if args.count {
            writeln!(out, "0").map_err(|e| e.to_string())?;
            return out.flush().map_err(|e| e.to_string());
        }
        return emit_paths(&mut out, &[], args.json, args.null);
    }

    // Service first (M2b), unless --index pins an index file.
    // Only a connection failure falls back; an answer (or a
    // typed service error) from a reachable service is final.
    let discovery = if args.index.is_none() {
        Some(service_client())
    } else {
        None
    };
    let (connected, failures) = discovery
        .map(|d| (d.connected, d.failures))
        .unwrap_or_default();
    if let Some((mut client, _endpoint)) = connected {
        if args.count {
            let n = client.count(query_text).map_err(|e| e.to_string())?;
            let n = match args.limit {
                Some(limit) => n.min(limit as u64),
                None => n,
            };
            writeln!(out, "{n}").map_err(|e| e.to_string())?;
            return out.flush().map_err(|e| e.to_string());
        }
        let limit = u32::try_from(args.limit.unwrap_or(0)).unwrap_or(u32::MAX);
        let sort = match args.sort {
            SortKey::Name => ferrow_ipc::Sort::Name,
            SortKey::Size => ferrow_ipc::Sort::Size,
            SortKey::Modified => ferrow_ipc::Sort::Modified,
            SortKey::Created => ferrow_ipc::Sort::Created,
            SortKey::Accessed => ferrow_ipc::Sort::Accessed,
        };
        let (rows, _) = client
            .query_rows(query_text, limit, sort, args.descending)
            .map_err(|e| e.to_string())?;
        let mut output = PathOutput::new(&mut out, args.json, args.null)?;
        for row in rows {
            output.row(
                &row.path,
                EntryMetadata {
                    size: row.size,
                    modified: row.modified,
                    created: row.created,
                    accessed: row.accessed,
                    attributes: row.attributes,
                },
                &args.columns,
            )?;
        }
        return output.finish();
    }

    for failure in failures {
        eprintln!(
            "ferrow: service at {}: {}; falling back to the per-user in-process index",
            failure.endpoint, failure.error
        );
    }
    let (index, _) = load(&args)?;

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
            sort: args.sort,
            descending: args.descending,
            limit: args.limit,
            ..SearchOptions::default()
        },
    );
    if result.cancelled {
        return Err("search cancelled".to_string());
    }
    if let Some(key) = result.sort_unavailable {
        return Err(format!(
            "{key:?} column unavailable; enable --index-columns and rebuild"
        ));
    }

    emit_hits(
        &mut out,
        &index,
        result.hits,
        args.json,
        args.null,
        &args.columns,
    )
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

/// `ferrow status` / `ferrow service status`: which mode is
/// active and why (SPEC section 5).
fn cmd_status() -> Result<(), String> {
    let discovery = service_client();
    match discovery.connected {
        Some((mut client, endpoint)) => {
            let (entries, mode, report) = client.status().map_err(|e| e.to_string())?;
            println!("mode: service ({endpoint})");
            println!("entries: {entries}");
            println!("backend: {mode} - {report}");
        }
        None => {
            for failure in discovery.failures {
                println!("service at {}: {}", failure.endpoint, failure.error);
            }
            let tried: Vec<String> = service_candidates().iter().map(|e| e.display()).collect();
            println!(
                "mode: in-process (no service reachable at {})",
                tried.join(", ")
            );
            match default_index_path() {
                Some(path) if path.exists() => println!("index: {}", path.display()),
                Some(path) => println!(
                    "index: {} (not built yet; run `ferrow index`)",
                    path.display()
                ),
                None => println!("index: no platform data directory; pass --index/--out"),
            }
        }
    }
    Ok(())
}

fn cmd_service(rest: &[std::ffi::OsString]) -> Result<(), String> {
    let Some(sub) = rest.first() else {
        return Err("service needs a subcommand: install | uninstall | status | run".into());
    };
    match sub.to_str().unwrap_or("") {
        "status" => cmd_status(),
        "run" => {
            let args = parse_args(&rest[1..])?;
            let endpoint = match std::env::var("FERROW_ENDPOINT_NAME") {
                Ok(name) if !name.is_empty() => Endpoint::named(&name),
                _ => Endpoint::user_service(),
            };
            let config = ferrowd::DaemonConfig {
                endpoint,
                index_path: args.index,
                roots: args
                    .positional
                    .iter()
                    .map(|p| PathBuf::from(p.clone()))
                    .collect(),
                excludes: args.excludes,
                follow_symlinks: args.follow_symlinks,
            };
            ferrowd::serve_foreground(&config).map_err(|e| e.to_string())
        }
        "install" => ferrowd::install::install().map(|msg| println!("{msg}")),
        "uninstall" => ferrowd::install::uninstall().map(|msg| println!("{msg}")),
        other => Err(format!(
            "unknown service subcommand: {other} (install | uninstall | status | run)"
        )),
    }
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
        "search" if argv.len() == 2 && matches!(argv[1].to_str(), Some("--help" | "-h")) => {
            print!("{USAGE}");
            Ok(())
        }
        "search" => cmd_search(parse_args(&argv[1..])?),
        "stats" => cmd_stats(parse_args(&argv[1..])?),
        "status" => cmd_status(),
        "service" => cmd_service(&argv[1..]),
        other => Err(format!("unknown subcommand: {other}\n\n{USAGE}")),
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

    #[cfg(unix)]
    #[test]
    fn app_data_dir_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("ferrow-private-dir-{}", std::process::id()));
        let dir = base.join("ferrow");
        let _ = std::fs::remove_dir_all(&base);
        create_private_dir_all(&dir).unwrap();
        for path in [&base, &dir] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} mode was {mode:o}", path.display());
        }
        // Existing directories keep their mode.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir_all(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Writer(Rc<RefCell<Vec<u8>>>);
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn fallback_writes_before_requesting_the_next_path() {
        let builder = ferrow_index::IndexBuilder::new();
        let root = builder.add_root(b"root", ferrow_index::flags::DIR);
        let first = builder.add(root, b"first", 0);
        let second = builder.add(root, b"second", 0);
        let index = builder.freeze();
        for (json, null) in [(false, false), (false, true), (true, false)] {
            let bytes = Rc::new(RefCell::new(Vec::new()));
            let observed = bytes.clone();
            let hits = std::iter::once(first).chain(std::iter::once_with(|| {
                assert!(
                    observed.borrow().len() > usize::from(json),
                    "first path was buffered"
                );
                second
            }));
            emit_hits(
                &mut Writer(bytes),
                &index,
                hits,
                json,
                null,
                &["path".into()],
            )
            .unwrap();
        }
    }
}
