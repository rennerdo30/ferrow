//! CI-only measurement harness: real unprivileged `ferrow service run`.
//! Usage: service_bench <ferrow binary> <saved fixture index>.
use ferrow_index::Index;
use ferrow_ipc::{Client, Endpoint};
use ferrow_query::{CancelToken, Query, SearchOptions, count, search};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        // Child is the exact process this harness created; always reap it.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn median(mut samples: Vec<Duration>) -> f64 {
    samples.sort_unstable();
    samples[samples.len() / 2].as_secs_f64() * 1000.0
}

fn main() {
    if let Err(error) = run() {
        eprintln!("service benchmark: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: service_bench <ferrow> <index>".into());
    }
    let index = Index::load(std::path::Path::new(&args[1]))?;
    let name = format!(
        "ferrow-bench-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let endpoint = Endpoint::named(&name);
    let started = Instant::now();
    let mut service = Service(
        Command::new(&args[0])
            .args(["service", "run", "--index"])
            .arg(&args[1])
            .env("FERROW_ENDPOINT_NAME", &name)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    eprintln!(
        "FERROW-SERVICE-ACTIVE pid={} mode=unprivileged-run startup_timeout_s=30",
        service.0.id()
    );
    let mut client = loop {
        if started.elapsed() >= Duration::from_secs(30) {
            return Err("service startup timed out".into());
        }
        if service.0.try_wait()?.is_some() {
            return Err("service exited during startup".into());
        }
        if let Ok(mut client) = Client::connect(&endpoint)
            && let Ok((entries, _, _)) = client.status()
        {
            if entries != index.len() as u64 {
                return Err("service loaded wrong fixture".into());
            }
            break client;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    println!(
        "FERROW-SERVICE-START entries={} ready_ms={:.3}",
        index.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    for text in [
        "a",
        "re",
        "report 2024",
        "!tmp log",
        "zzzz-no-hit-zzzz",
        "文書",
    ] {
        let query = Query::parse(text)?;
        let expected = search(
            &index,
            &query,
            &SearchOptions {
                limit: Some(100),
                ..SearchOptions::default()
            },
        );
        let expected_paths: Vec<Vec<u8>> = expected
            .hits
            .iter()
            .map(|&id| {
                let mut path = Vec::new();
                index.path_bytes(id, &mut path);
                path
            })
            .collect();
        let (total, cancelled) = count(&index, &query, &CancelToken::new());
        if cancelled {
            return Err("reference count cancelled".into());
        }
        let mut page = Vec::new();
        let mut full = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            let paths = client.query(text, 100)?;
            page.push(t.elapsed());
            if paths != expected_paths {
                return Err(format!("IPC order/content for {text:?}").into());
            }
            let t = Instant::now();
            let got = client.count(text)?;
            full.push(t.elapsed());
            if got != total as u64 {
                return Err(format!("IPC count for {text:?}").into());
            }
        }
        println!(
            "FERROW-SERVICE query={text:?} first_page_ms={:.6} page_hits={} full_count_ms={:.6} total_hits={total}",
            median(page),
            expected_paths.len(),
            median(full)
        );
    }
    drop(client);
    // Every normal IPC operation has its existing five-second deadline.
    // The CI step also imposes an overall harness timeout.
    Ok(())
}
