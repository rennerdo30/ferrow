//! Selective paired CI latency retry. Built against each arm's libraries.
use ferrow_index::Index;
use ferrow_ipc::{Client, Endpoint};
use ferrow_query::{CancelToken, Query, SearchOptions, count, search};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Service(Child);
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 5 {
        return Err("expected binary, index, kind, query, operation".into());
    }
    let path = std::path::Path::new(&args[1]);
    let mut samples = Vec::new();
    let mut hits = 0;
    if args[2] == "load" {
        for _ in 0..3 {
            let (index, timing) = Index::load_with_timings(path)?;
            hits = index.len();
            samples.push(timing.total);
        }
    } else {
        let index = Index::load(path)?;
        let query = Query::parse(&args[3])?;
        let page = args[4] == "first_page_ms";
        if !page && args[4] != "full_count_ms" {
            return Err("unknown operation".into());
        }
        let options = SearchOptions {
            limit: Some(100),
            ..SearchOptions::default()
        };
        match args[2].as_str() {
            "in-process" => {
                for _ in 0..5 {
                    let started = Instant::now();
                    hits = if page {
                        let result = search(&index, &query, &options);
                        if result.cancelled {
                            return Err("cancelled page".into());
                        }
                        result.hits.len()
                    } else {
                        let (n, cancelled) = count(&index, &query, &CancelToken::new());
                        if cancelled {
                            return Err("cancelled count".into());
                        }
                        n
                    };
                    samples.push(started.elapsed());
                }
            }
            "IPC" => {
                let name = format!("ferrow-retry-{}", std::process::id());
                let endpoint = Endpoint::named(&name);
                let mut service = Service(
                    Command::new(&args[0])
                        .args(["service", "run", "--index"])
                        .arg(path)
                        .env("FERROW_ENDPOINT_NAME", name)
                        .stdout(Stdio::null())
                        .stderr(Stdio::inherit())
                        .spawn()?,
                );
                eprintln!("FERROW-RETRY-ACTIVE pid={} timeout_s=120", service.0.id());
                let startup = Instant::now();
                let mut client = loop {
                    if startup.elapsed() > Duration::from_secs(30)
                        || service.0.try_wait()?.is_some()
                    {
                        return Err("retry service startup failed".into());
                    }
                    if let Ok(mut client) = Client::connect(&endpoint)
                        && let Ok((n, _, _)) = client.status()
                    {
                        if n != index.len() as u64 {
                            return Err("wrong fixture".into());
                        }
                        break client;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                let expected = search(&index, &query, &options);
                let expected_paths: Vec<_> = expected
                    .hits
                    .iter()
                    .map(|&id| {
                        let mut bytes = Vec::new();
                        index.path_bytes(id, &mut bytes);
                        bytes
                    })
                    .collect();
                let (total, _) = count(&index, &query, &CancelToken::new());
                for _ in 0..5 {
                    let started = Instant::now();
                    if page {
                        let paths = client.query(&args[3], 100)?;
                        samples.push(started.elapsed());
                        if paths != expected_paths {
                            return Err("retry content/order mismatch".into());
                        }
                        hits = paths.len();
                    } else {
                        let n = client.count(&args[3])?;
                        samples.push(started.elapsed());
                        if n != total as u64 {
                            return Err("retry count mismatch".into());
                        }
                        hits = n as usize;
                    }
                }
            }
            _ => return Err("unknown retry kind".into()),
        }
    }
    samples.sort_unstable();
    println!(
        "FERROW-RETRY value_ms={:.6} hits={hits}",
        samples[samples.len() / 2].as_secs_f64() * 1000.0
    );
    Ok(())
}
