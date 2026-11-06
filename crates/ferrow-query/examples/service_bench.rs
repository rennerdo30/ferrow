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
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    assert_eq!(args.len(), 2, "usage: service_bench <ferrow> <index>");
    let index = Index::load(std::path::Path::new(&args[1])).expect("load reference fixture");
    let name = format!(
        "ferrow-bench-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
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
            .spawn()
            .expect("start service run"),
    );
    eprintln!(
        "FERROW-SERVICE-ACTIVE pid={} mode=unprivileged-run startup_timeout_s=30",
        service.0.id()
    );
    let mut client = loop {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "service startup timed out"
        );
        assert!(
            service.0.try_wait().unwrap().is_none(),
            "service exited during startup"
        );
        if let Ok(mut client) = Client::connect(&endpoint)
            && let Ok((entries, _, _)) = client.status()
        {
            assert_eq!(entries, index.len() as u64);
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
        let query = Query::parse(text).unwrap();
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
        assert!(!cancelled);
        let mut page = Vec::new();
        let mut full = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            let paths = client.query(text, 100).expect("IPC first page");
            page.push(t.elapsed());
            assert_eq!(paths, expected_paths, "IPC order/content for {text:?}");
            let t = Instant::now();
            let got = client.count(text).expect("IPC count");
            full.push(t.elapsed());
            assert_eq!(got, total as u64, "IPC count for {text:?}");
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
}
