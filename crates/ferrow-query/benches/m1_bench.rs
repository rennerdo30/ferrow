//! M1 benchmark over the synthetic fixture (CI only at full size).
//!
//! Before the divan benches run, `main` prints one-shot measures as
//! `FERROW-MEASURE key=value` and `FERROW-QUERY ...` lines; the CI bench
//! job turns those into its Markdown summary table. Set
//! `FERROW_BENCH_ENTRIES` to change the fixture size (default 5,000,000;
//! keep local runs at or below 100,000).

use std::hint::black_box;
use std::sync::OnceLock;
use std::time::Instant;

use ferrow_fixture::generate_into;
use ferrow_index::{Index, IndexBuilder};
use ferrow_query::{
    CancelToken, Query, QueryTimings, SearchOptions, count, count_with_timings, search,
    search_with_timings,
};

static INDEX: OnceLock<Index> = OnceLock::new();

const SEED: u64 = 2026;
const QUERIES: &[&str] = &[
    "a",
    "re",
    "report 2024",
    "!tmp log",
    "zzzz-no-hit-zzzz",
    "文書",
];

fn entries() -> usize {
    std::env::var("FERROW_BENCH_ENTRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000_000)
}

fn median(mut runs: Vec<QueryTimings>) -> QueryTimings {
    runs.sort_by_key(|t| t.total);
    runs[runs.len() / 2]
}

fn breakdown(text: &str, operation: &str, t: QueryTimings) {
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    println!(
        "FERROW-QUERY-PARTS query={text:?} operation={operation} total_ms={:.6} prepare_ms={:.6} scan_ms={:.6} collect_ms={:.6} unattributed_ms={:.6}",
        ms(t.total),
        ms(t.prepare),
        ms(t.scan),
        ms(t.collect),
        ms(t.unattributed())
    );
}

fn measures() {
    let n = entries();
    let builder = IndexBuilder::new();
    let t = Instant::now();
    generate_into(&builder, n, SEED);
    let generate_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let index = builder.freeze();
    let freeze_ms = t.elapsed().as_secs_f64() * 1000.0;

    let heap = index.heap_bytes();
    println!(
        "FERROW-MEASURE entries={n} seed={SEED} generate_ms={generate_ms:.1} \
         freeze_ms={freeze_ms:.1} heap_bytes={heap} bytes_per_entry={:.2}",
        heap as f64 / index.len() as f64
    );

    // Persisted size and load time.
    let retained = std::env::var_os("FERROW_BENCH_INDEX_PATH");
    let path = retained
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("ferrow-bench-{}.bin", std::process::id()))
        });
    index.save(&path).expect("save bench index");
    let file_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    // Load breakdown from the median-total run, so the named parts
    // plus the unattributed remainder add up to that run's total.
    let mut load_runs = Vec::new();
    for _ in 0..3 {
        let (loaded, timings) = Index::load_with_timings(&path).expect("load bench index");
        assert_eq!(loaded.len(), index.len());
        load_runs.push(timings);
        drop(loaded);
    }
    if retained.is_none() {
        let _ = std::fs::remove_file(&path);
    }
    load_runs.sort_by_key(|t| t.total);
    let load = load_runs[load_runs.len() / 2];
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    println!(
        "FERROW-MEASURE file_bytes={file_bytes} load_ms_median={:.1} \
         load_read_ms={:.1} load_checksum_ms={:.1} load_decode_ms={:.1} \
         load_structure_ms={:.1} load_derived_ms={:.1} load_unattributed_ms={:.1} load_buffer_ms={:.1} load_cleanup_ms={:.1} load_fold_ms={:.1} load_sort_ms={:.1}",
        ms(load.total),
        ms(load.read),
        ms(load.checksum),
        ms(load.decode),
        ms(load.structure),
        ms(load.derived),
        ms(load.unattributed()),
        ms(load.buffer),
        ms(load.cleanup),
        ms(load.derived_fold),
        ms(load.derived_sort),
    );

    // Query latency: first page (limit 100) and full count, median of 5.
    for text in QUERIES {
        let query = Query::parse(text).expect("bench query parses");
        let mut page = Vec::new();
        let mut hits = 0usize;
        for _ in 0..5 {
            let (r, timings) = search_with_timings(
                &index,
                &query,
                &SearchOptions {
                    limit: Some(100),
                    ..SearchOptions::default()
                },
            );
            page.push(timings);
            hits = r.hits.len();
        }
        let mut full = Vec::new();
        let mut total = 0usize;
        for _ in 0..5 {
            let ((c, _), timings) = count_with_timings(&index, &query, &CancelToken::new());
            full.push(timings);
            total = c;
        }
        let page = median(page);
        let full = median(full);
        breakdown(text, "page", page);
        breakdown(text, "count", full);
        println!(
            "FERROW-QUERY query={text:?} first_page_ms={:.3} page_hits={hits} \
             full_count_ms={:.3} total_hits={total}",
            page.total.as_secs_f64() * 1000.0,
            full.total.as_secs_f64() * 1000.0
        );
    }

    INDEX.set(index).expect("bench index set once");
}

fn index() -> &'static Index {
    INDEX
        .get()
        .expect("measures() builds the index before divan runs")
}

#[divan::bench(args = QUERIES)]
fn first_page(bencher: divan::Bencher, text: &str) {
    let query = Query::parse(text).unwrap();
    bencher.bench(|| {
        black_box(search(
            black_box(index()),
            &query,
            &SearchOptions {
                limit: Some(100),
                ..SearchOptions::default()
            },
        ))
    });
}

#[divan::bench(args = QUERIES)]
fn full_count(bencher: divan::Bencher, text: &str) {
    let query = Query::parse(text).unwrap();
    bencher.bench(|| black_box(count(black_box(index()), &query, &CancelToken::new())));
}

fn main() {
    println!(
        "FERROW-ACTIVE query=arena-count-window crc=crc32fast derived=direct-ascii-carried-keys freeze=worker-scratch"
    );
    measures();
    if std::env::var_os("FERROW_MEASURES_ONLY").is_none() {
        divan::main();
    }
}
